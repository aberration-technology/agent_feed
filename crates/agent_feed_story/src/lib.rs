use agent_feed_core::{
    AgentEvent, Bulletin, BulletinChip, BulletinId, BulletinMode, EventId, EventKind, PrivacyClass,
    Severity, TickerItem, VisualKind,
};
use agent_feed_highlight::score_event;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use time::OffsetDateTime;

const DEFAULT_DWELL_MS: u64 = 14_000;
const URGENT_DWELL_MS: u64 = 20_000;
const DEFAULT_MIN_SCORE: u8 = 65;
const DEFAULT_MIN_CONTEXT_SCORE: u8 = 70;
const DEFAULT_DEDUPE_WINDOW: usize = 32;
const DEFAULT_DECISION_HISTORY: usize = 40;
const STARTUP_CONTEXT_TAG: &str = "startup-context";

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoryDecisionAction {
    Waiting,
    Retained,
    Rejected,
    Deduped,
    Published,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoryDecision {
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub action: StoryDecisionAction,
    pub reason: String,
    pub agent: String,
    pub project: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub family: StoryFamily,
    pub score: u8,
    pub context_score: u8,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoryCompilerDiagnostics {
    pub open_windows: usize,
    pub retained_windows: usize,
    pub settled_windows: usize,
    pub published_stories: usize,
    pub rejected_stories: usize,
    pub deduped_stories: usize,
    pub last_decision: Option<StoryDecision>,
    pub recent_decisions: Vec<StoryDecision>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoryUpdateSignature {
    pub topic_fingerprint: String,
    pub state_fingerprint: String,
    pub impact_fingerprint: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoryUpdateRelation {
    NewTopic,
    ExactRepeat,
    SameState,
    StateChanged,
    ImpactChanged,
}

impl StoryUpdateRelation {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewTopic => "new_topic",
            Self::ExactRepeat => "exact_repeat",
            Self::SameState => "same_state",
            Self::StateChanged => "state_changed",
            Self::ImpactChanged => "impact_changed",
        }
    }
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
    pub command_topics: Vec<String>,
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
    #[serde(default)]
    pub publishable_events: usize,
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
            publishable_events: 0,
        }
    }

    fn observe(&mut self, event: &AgentEvent) {
        self.observe_with_publishable(event, true);
    }

    fn observe_context(&mut self, event: &AgentEvent) {
        self.observe_with_publishable(event, false);
    }

    fn observe_with_publishable(&mut self, event: &AgentEvent, publishable: bool) {
        self.last_event_at = event.occurred_at.unwrap_or(event.received_at);
        self.events.push(event.id.clone());
        self.counters.events += 1;
        if publishable {
            self.publishable_events += 1;
        }
        self.signals.highest_score = self.signals.highest_score.max(score_event(event));
        if severity_rank(event.severity) > severity_rank(self.signals.highest_severity) {
            self.signals.highest_severity = event.severity;
        }
        self.signals.latest_kind = Some(event.kind);
        self.signals.latest_title = Some(event.title.clone());
        if let Some(summary) = event.summary.as_ref()
            && !summary_is_redundant(summary)
        {
            self.signals.latest_summary = Some(summary.clone());
        }
        self.signals.latest_tool.clone_from(&event.tool);
        if let Some(command) = event.command.as_deref().and_then(command_class) {
            self.signals.latest_command_class = Some(command);
        }
        if let Some(topic) = event.command.as_deref().and_then(command_topic)
            && !self
                .signals
                .command_topics
                .iter()
                .any(|existing| existing == topic)
        {
            self.signals.command_topics.push(topic.to_string());
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
    active_update_fingerprints: BTreeMap<StoryKey, String>,
    diagnostics: StoryCompilerDiagnostics,
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
            active_update_fingerprints: BTreeMap::new(),
            diagnostics: StoryCompilerDiagnostics::default(),
        }
    }

    #[must_use]
    pub fn diagnostics(&self) -> StoryCompilerDiagnostics {
        let mut diagnostics = self.diagnostics.clone();
        diagnostics.open_windows = self.windows.len();
        diagnostics
    }

    #[must_use]
    pub fn ingest(&mut self, event: AgentEvent) -> Vec<CompiledStory> {
        let primary_key = story_key(&event);
        let context_only = is_startup_context(&event);
        if primary_key.family != StoryFamily::Turn {
            self.touch_turn_rollup(&event, !context_only);
        }

        if context_only {
            self.record_event_decision(
                &event,
                primary_key.family,
                StoryDecisionAction::Waiting,
                "startup context is waiting for future activity",
            );
            if primary_key.family == StoryFamily::Turn {
                self.touch_window_with_key(primary_key, event, false);
            }
            return Vec::new();
        }

        if is_never_publish_event(&event) {
            self.record_event_decision(
                &event,
                primary_key.family,
                StoryDecisionAction::Waiting,
                "waiting for a completion, test, edit, or incident signal",
            );
            self.touch_window_with_key(primary_key, event, true);
            return Vec::new();
        }

        let now = event.occurred_at.unwrap_or(event.received_at);
        let mut window = self
            .windows
            .remove(&primary_key)
            .unwrap_or_else(|| StoryWindow::new(primary_key.clone(), now));
        window.observe(&event);

        if !should_settle(&event, &window) {
            self.record_window_decision(
                &window,
                StoryDecisionAction::Waiting,
                "window is still gathering context",
            );
            self.windows.insert(primary_key, window);
            return Vec::new();
        }

        window.state = StoryWindowState::Settled;
        self.diagnostics.settled_windows += 1;
        if let Some(rejection) = self.compile_rejection(&window) {
            self.record_window_rejection(&window, rejection);
            return Vec::new();
        }
        let Some(story) = self.compile_window(window) else {
            return Vec::new();
        };
        if self.accept_story(&story) {
            self.diagnostics.published_stories += 1;
            self.record_story_decision(&story, StoryDecisionAction::Published, "story emitted");
            vec![story]
        } else {
            self.diagnostics.deduped_stories += 1;
            self.record_story_decision(
                &story,
                StoryDecisionAction::Deduped,
                "similar story was already published recently",
            );
            Vec::new()
        }
    }

    #[must_use]
    pub fn flush(&mut self) -> Vec<CompiledStory> {
        let windows = std::mem::take(&mut self.windows);
        let mut stories = Vec::new();
        for (key, mut window) in windows {
            if window.publishable_events == 0 {
                self.diagnostics.retained_windows += 1;
                self.record_window_decision(
                    &window,
                    StoryDecisionAction::Retained,
                    "startup context is waiting for future activity",
                );
                self.windows.insert(key, window);
                continue;
            }
            if is_open_turn_window(&window) {
                if let Some(story) = self.compile_active_update(&window) {
                    let fingerprint = active_update_fingerprint(&window, &story);
                    let changed = self
                        .active_update_fingerprints
                        .get(&key)
                        .is_none_or(|existing| existing != &fingerprint);
                    if changed && self.accept_story_with_extra(&story, Some(&fingerprint)) {
                        self.active_update_fingerprints
                            .insert(key.clone(), fingerprint);
                        self.diagnostics.published_stories += 1;
                        self.record_story_decision(
                            &story,
                            StoryDecisionAction::Published,
                            "active update emitted",
                        );
                        stories.push(story);
                    } else {
                        self.diagnostics.deduped_stories += 1;
                        self.record_story_decision(
                            &story,
                            StoryDecisionAction::Deduped,
                            "active update did not meaningfully change",
                        );
                    }
                    self.windows.insert(key, window);
                    continue;
                }
                if should_retain_open_window(&window) {
                    self.diagnostics.retained_windows += 1;
                    self.record_window_decision(
                        &window,
                        StoryDecisionAction::Retained,
                        "open turn is waiting for a meaningful outcome update",
                    );
                    self.windows.insert(key, window);
                    continue;
                }
            }
            if let Some(rejection) = self.compile_rejection(&window) {
                if rejection.score < self.config.min_score && should_retain_open_window(&window) {
                    self.diagnostics.retained_windows += 1;
                    self.record_window_decision(
                        &window,
                        StoryDecisionAction::Retained,
                        "waiting for stronger story signal before publishing",
                    );
                    self.windows.insert(key, window);
                } else {
                    self.record_window_rejection(&window, rejection);
                }
                continue;
            }
            window.state = StoryWindowState::Settled;
            self.diagnostics.settled_windows += 1;
            let Some(story) = self.compile_window(window) else {
                continue;
            };
            if self.accept_story(&story) {
                self.active_update_fingerprints.remove(&key);
                self.diagnostics.published_stories += 1;
                self.record_story_decision(&story, StoryDecisionAction::Published, "story emitted");
                stories.push(story);
            } else {
                self.diagnostics.deduped_stories += 1;
                self.record_story_decision(
                    &story,
                    StoryDecisionAction::Deduped,
                    "similar story was already published recently",
                );
            }
        }
        stories
    }

    fn touch_window_with_key(&mut self, key: StoryKey, event: AgentEvent, publishable: bool) {
        let now = event.occurred_at.unwrap_or(event.received_at);
        let window = self
            .windows
            .entry(key.clone())
            .or_insert_with(|| StoryWindow::new(key, now));
        if publishable {
            window.observe(&event);
        } else {
            window.observe_context(&event);
        }
    }

    fn touch_turn_rollup(&mut self, event: &AgentEvent, publishable: bool) {
        if event.session_id.is_none() && event.turn_id.is_none() {
            return;
        }
        let key = StoryKey {
            feed_id: None,
            agent: event.agent.clone(),
            project_hash: event.project.clone(),
            session_id: event.session_id.clone(),
            turn_id: event.turn_id.clone(),
            family: StoryFamily::Turn,
        };
        let now = event.occurred_at.unwrap_or(event.received_at);
        let window = self
            .windows
            .entry(key.clone())
            .or_insert_with(|| StoryWindow::new(key, now));
        if publishable {
            window.observe(event);
        } else {
            window.observe_context(event);
        }
    }

    fn compile_window(&self, window: StoryWindow) -> Option<CompiledStory> {
        if self.compile_rejection(&window).is_some() {
            return None;
        }
        self.compile_window_with_scores(window, None, None)
    }

    fn compile_active_update(&self, window: &StoryWindow) -> Option<CompiledStory> {
        if let Some(summary) = window.signals.latest_summary.as_deref() {
            if !active_update_summary(summary) && !active_progress_summary(summary) {
                return self.compile_activity_checkpoint(window);
            }
            let score = story_score(window).max(72);
            let context_score = context_score(window).max(76);
            let story = self.compile_window_with_scores(
                window.clone(),
                Some(score.min(88)),
                Some(context_score.min(100)),
            )?;
            if is_low_quality_story(window, &story.headline, &story.deck)
                && !active_summary_has_release_outcome(summary)
            {
                return self.compile_activity_checkpoint(window);
            }
            return Some(story);
        }
        self.compile_activity_checkpoint(window)
    }

    fn compile_activity_checkpoint(&self, window: &StoryWindow) -> Option<CompiledStory> {
        let (headline, deck) = activity_checkpoint_copy(window)?;
        let score = story_score(window).clamp(72, 86);
        let context_score = context_score(window).clamp(76, 100);
        self.compile_story_with_copy(window.clone(), headline, deck, score, context_score)
    }

    fn compile_window_with_scores(
        &self,
        window: StoryWindow,
        score_override: Option<u8>,
        context_score_override: Option<u8>,
    ) -> Option<CompiledStory> {
        let score = story_score(&window);
        let score = score_override.unwrap_or(score);
        let context_score = context_score_override.unwrap_or_else(|| context_score(&window));
        let mut headline = headline(&window);
        let mut deck = deck(&window);
        if let Some((rewritten_headline, rewritten_deck)) =
            story_impact_rewrite(&window, &headline, &deck)
        {
            headline = rewritten_headline;
            deck = rewritten_deck;
        }
        self.compile_story_with_copy(window, headline, deck, score, context_score)
    }

    fn compile_story_with_copy(
        &self,
        window: StoryWindow,
        headline: String,
        deck: String,
        score: u8,
        context_score: u8,
    ) -> Option<CompiledStory> {
        let project = window.key.project_hash.clone();
        let mut chips = Vec::new();
        if let Some(project) = &project {
            chips.push(project.clone());
        }
        chips.push(window.key.agent.clone());
        chips.push(story_family_label(window.key.family).to_string());
        if window.counters.files_changed > 0 {
            chips.push(format!("{} files", window.counters.files_changed));
        }
        chips.push(format!("score {score}"));
        chips.push("redacted".to_string());
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

    fn compile_rejection(&self, window: &StoryWindow) -> Option<StoryCompileRejection> {
        let score = story_score(window);
        let context_score = context_score(window);
        if score < self.config.min_score {
            return Some(StoryCompileRejection {
                reason: "score below publish threshold",
                score,
                context_score,
            });
        }
        if context_score < self.config.min_context_score {
            return Some(StoryCompileRejection {
                reason: "context below publish threshold",
                score,
                context_score,
            });
        }

        let mut headline = headline(window);
        let mut deck = deck(window);
        if let Some((rewritten_headline, rewritten_deck)) =
            story_impact_rewrite(window, &headline, &deck)
        {
            headline = rewritten_headline;
            deck = rewritten_deck;
        }
        if is_low_quality_story(window, &headline, &deck) {
            return Some(StoryCompileRejection {
                reason: "summary was too generic or mechanical to publish",
                score,
                context_score,
            });
        }
        None
    }

    fn accept_story(&mut self, story: &CompiledStory) -> bool {
        self.accept_story_with_extra(story, None)
    }

    fn accept_story_with_extra(&mut self, story: &CompiledStory, extra: Option<&str>) -> bool {
        let fingerprint = format!(
            "{}:{}:{:?}:{}:{}",
            story.agent,
            story.project.as_deref().unwrap_or("local"),
            story.family,
            semantic_story_fingerprint(&story.headline, &story.deck),
            extra.unwrap_or_default()
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

    fn record_decision(&mut self, decision: StoryDecision) {
        self.diagnostics.last_decision = Some(decision.clone());
        self.diagnostics.recent_decisions.insert(0, decision);
        if self.diagnostics.recent_decisions.len() > DEFAULT_DECISION_HISTORY {
            self.diagnostics
                .recent_decisions
                .truncate(DEFAULT_DECISION_HISTORY);
        }
    }

    fn record_event_decision(
        &mut self,
        event: &AgentEvent,
        family: StoryFamily,
        action: StoryDecisionAction,
        reason: &'static str,
    ) {
        self.record_decision(StoryDecision {
            at: event.occurred_at.unwrap_or(event.received_at),
            action,
            reason: reason.to_string(),
            agent: event.agent.clone(),
            project: event.project.clone(),
            session_id: event.session_id.clone(),
            turn_id: event.turn_id.clone(),
            family,
            score: score_event(event),
            context_score: 0,
        });
    }

    fn record_window_decision(
        &mut self,
        window: &StoryWindow,
        action: StoryDecisionAction,
        reason: &'static str,
    ) {
        self.record_decision(StoryDecision {
            at: window.last_event_at,
            action,
            reason: reason.to_string(),
            agent: window.key.agent.clone(),
            project: window.key.project_hash.clone(),
            session_id: window.key.session_id.clone(),
            turn_id: window.key.turn_id.clone(),
            family: window.key.family,
            score: story_score(window),
            context_score: context_score(window),
        });
    }

    fn record_window_rejection(&mut self, window: &StoryWindow, rejection: StoryCompileRejection) {
        self.diagnostics.rejected_stories += 1;
        self.record_decision(StoryDecision {
            at: window.last_event_at,
            action: StoryDecisionAction::Rejected,
            reason: rejection.reason.to_string(),
            agent: window.key.agent.clone(),
            project: window.key.project_hash.clone(),
            session_id: window.key.session_id.clone(),
            turn_id: window.key.turn_id.clone(),
            family: window.key.family,
            score: rejection.score,
            context_score: rejection.context_score,
        });
    }

    fn record_story_decision(
        &mut self,
        story: &CompiledStory,
        action: StoryDecisionAction,
        reason: &'static str,
    ) {
        self.record_decision(StoryDecision {
            at: story.created_at,
            action,
            reason: reason.to_string(),
            agent: story.agent.clone(),
            project: story.project.clone(),
            session_id: story.key.session_id.clone(),
            turn_id: story.key.turn_id.clone(),
            family: story.family,
            score: story.score,
            context_score: story.context_score,
        });
    }
}

#[derive(Clone, Copy, Debug)]
struct StoryCompileRejection {
    reason: &'static str,
    score: u8,
    context_score: u8,
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

fn should_retain_open_window(window: &StoryWindow) -> bool {
    window.key.family == StoryFamily::Turn
        && window.signals.highest_score >= 30
        && window.counters.events < 128
}

fn is_open_turn_window(window: &StoryWindow) -> bool {
    window.key.family == StoryFamily::Turn
        && window.state == StoryWindowState::Open
        && window
            .signals
            .latest_kind
            .is_none_or(|kind| !matches!(kind, EventKind::TurnComplete | EventKind::TurnFail))
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
            | EventKind::AdapterHealth
    ) || (event.kind == EventKind::AgentMessage
        && event.summary.as_deref().is_none_or(|summary| {
            !meaningful_summary(summary) || !summary_has_work_context(summary)
        }))
}

fn is_startup_context(event: &AgentEvent) -> bool {
    event.tags.iter().any(|tag| tag == STARTUP_CONTEXT_TAG)
}

fn context_score(window: &StoryWindow) -> u8 {
    let mut score = 0u8;
    let story_score = story_score(window);
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
    if story_score >= DEFAULT_MIN_SCORE {
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

fn story_score(window: &StoryWindow) -> u8 {
    let mut score = window.signals.highest_score;
    if window.key.family == StoryFamily::Turn
        && window
            .signals
            .latest_summary
            .as_deref()
            .is_some_and(meaningful_summary)
    {
        score = score.max(74);
    }
    if has_command_burst(window) {
        score = score.max(68 + window.counters.commands.min(8) as u8);
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
        StoryFamily::Incident => {
            if window
                .signals
                .latest_kind
                .is_some_and(|kind| kind == EventKind::TurnFail)
            {
                if window
                    .signals
                    .latest_summary
                    .as_deref()
                    .is_some_and(|summary| summary.to_ascii_lowercase().contains("interrupt"))
                {
                    format!("{agent} was interrupted")
                } else {
                    format!("{agent} turn failed")
                }
            } else {
                format!("{agent} hit {}", object)
            }
        }
        StoryFamily::Mcp => format!("{agent} saw mcp degradation"),
        StoryFamily::Plan => format!("{agent} updated the plan"),
        StoryFamily::Command => format!("{agent} completed {object}"),
        StoryFamily::IdleRecap => format!("{agent} activity settled"),
        StoryFamily::Turn => turn_headline(window, agent, &object),
    }
}

fn deck(window: &StoryWindow) -> String {
    let mut parts = Vec::new();
    let mut has_meaningful_turn_summary = false;
    if window.key.family == StoryFamily::Turn
        && let Some(summary) = &window.signals.latest_summary
        && !summary.is_empty()
        && meaningful_summary(summary)
    {
        parts.push(safe_sentence(summary));
        has_meaningful_turn_summary = true;
    }
    if let Some(deck) = command_burst_deck(window)
        && parts.is_empty()
    {
        parts.push(deck);
    }
    if window.counters.files_changed > 0 && !has_meaningful_turn_summary {
        parts.push(format!(
            "{} changed files",
            window.counters.files_changed.min(99)
        ));
    }
    if window.counters.tests_failed > 0 && !has_meaningful_turn_summary {
        parts.push("tests are red".to_string());
    } else if window.counters.tests_passed > 0 && !has_meaningful_turn_summary {
        parts.push("tests passed".to_string());
    }
    if window.counters.tool_failures > 0 && !has_meaningful_turn_summary {
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
        && window.key.family != StoryFamily::Turn
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
    if let Some(project) = &window.key.project_hash {
        let mut parts = vec![project.clone(), window.key.agent.clone()];
        parts.push(story_family_label(window.key.family).to_string());
        parts.push(format!("score {score}"));
        parts.push("redacted".to_string());
        return parts.join(" · ");
    }
    let mut parts = vec![window.key.agent.clone(), "local".to_string()];
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

fn turn_headline(window: &StoryWindow, agent: &str, object: &str) -> String {
    if let Some(summary) = window
        .signals
        .latest_summary
        .as_deref()
        .and_then(|summary| summary_headline(agent, summary))
    {
        return summary;
    }
    if window.counters.tests_failed > 0 {
        return format!("{agent} found failing tests");
    }
    if window.counters.tests_passed > 0 && window.counters.files_changed > 0 {
        return format!("{agent} verified the update");
    }
    if window.counters.tests_passed > 0 {
        return format!("{agent} verified tests");
    }
    if window.counters.files_changed > 0 {
        return format!(
            "{agent} finished {} file update",
            window.counters.files_changed.min(99)
        );
    }
    if let Some(label) = command_burst_label(window) {
        return format!("{agent} {label}");
    }
    format!("{agent} completed {object}")
}

fn summary_headline(agent: &str, summary: &str) -> Option<String> {
    if !meaningful_summary(summary) {
        return None;
    }
    let sentence = safe_sentence(summary);
    let normalized = sentence.trim_matches(['.', '!', '?']).trim();
    if normalized.is_empty() {
        return None;
    }
    let lowered = normalized.to_ascii_lowercase();
    if [
        "done",
        "completed",
        "turn completed",
        "task complete",
        "finished",
        "ok",
    ]
    .iter()
    .any(|generic| lowered == *generic)
    {
        return None;
    }
    let without_agent = strip_agent_prefix(normalized, agent);
    let without_pronoun = without_agent
        .strip_prefix("I ")
        .or_else(|| without_agent.strip_prefix("i "))
        .unwrap_or(without_agent);
    let first = without_pronoun
        .split_whitespace()
        .take(10)
        .collect::<Vec<_>>()
        .join(" ");
    if first.is_empty() {
        None
    } else {
        Some(lower_initial(&first))
    }
}

fn strip_agent_prefix<'a>(input: &'a str, agent: &str) -> &'a str {
    let lowered = input.to_ascii_lowercase();
    for prefix in [
        format!("{} ", agent.to_ascii_lowercase()),
        "codex ".to_string(),
        "claude ".to_string(),
        "agent ".to_string(),
    ] {
        if lowered.starts_with(&prefix) {
            return input[prefix.len()..].trim_start();
        }
    }
    input
}

fn lower_initial(input: &str) -> String {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_lowercase().chain(chars).collect()
}

fn is_low_quality_story(window: &StoryWindow, headline: &str, deck: &str) -> bool {
    let combined = format!(
        "{} {}",
        headline.to_ascii_lowercase(),
        deck.to_ascii_lowercase()
    );
    if window
        .signals
        .latest_summary
        .as_deref()
        .map(normalized_story_text)
        .as_deref()
        .is_some_and(summary_is_operator_chatter)
    {
        return true;
    }
    if window.key.family == StoryFamily::FileChange
        && !window
            .signals
            .latest_summary
            .as_deref()
            .is_some_and(meaningful_summary)
    {
        return true;
    }
    if window.key.family == StoryFamily::Test
        && window.counters.tests_failed == 0
        && !window
            .signals
            .latest_summary
            .as_deref()
            .is_some_and(meaningful_summary)
    {
        return true;
    }
    if window.key.family == StoryFamily::Plan
        && window
            .signals
            .latest_summary
            .as_deref()
            .is_none_or(|summary| !meaningful_summary(summary))
    {
        return true;
    }
    let only_tool_failure = window.key.family == StoryFamily::Incident
        && window.counters.tool_failures > 0
        && window.counters.files_changed == 0
        && window.counters.tests_failed == 0
        && window.counters.tests_passed == 0
        && window.counters.permissions == 0
        && window.counters.mcp_failures == 0;
    if only_tool_failure
        && (combined.contains("shell command")
            || combined.contains("command failed")
            || combined.contains("tool failures"))
    {
        return true;
    }
    let turn_only_tool_failure = window.key.family == StoryFamily::Turn
        && window.counters.tool_failures > 0
        && window.counters.files_changed == 0
        && window.counters.tests_failed == 0
        && window.counters.tests_passed == 0
        && window.counters.permissions == 0
        && window.counters.mcp_failures == 0
        && window
            .signals
            .latest_summary
            .as_deref()
            .is_none_or(|summary| !meaningful_summary(summary));
    if turn_only_tool_failure {
        return true;
    }
    let turn_only_mechanics_without_meaningful_summary = window.key.family == StoryFamily::Turn
        && window.counters.permissions == 0
        && window.counters.mcp_failures == 0
        && (window.counters.files_changed > 0
            || window.counters.tests_failed > 0
            || window.counters.tests_passed > 0
            || window.counters.tool_failures > 0)
        && window
            .signals
            .latest_summary
            .as_deref()
            .is_none_or(|summary| !meaningful_summary(summary));
    if turn_only_mechanics_without_meaningful_summary {
        return true;
    }
    let generic_turn_without_signal = window.key.family == StoryFamily::Turn
        && window.counters.tool_failures == 0
        && window.counters.files_changed == 0
        && window.counters.tests_failed == 0
        && window.counters.tests_passed == 0
        && window.counters.permissions == 0
        && window.counters.mcp_failures == 0
        && window
            .signals
            .latest_summary
            .as_deref()
            .is_none_or(summary_is_redundant);
    if generic_turn_without_signal {
        return true;
    }
    let command_burst_without_meaningful_summary = has_command_burst(window)
        && window
            .signals
            .latest_summary
            .as_deref()
            .is_none_or(|summary| {
                !meaningful_summary(summary) || !summary_has_work_context(summary)
            });
    if command_burst_without_meaningful_summary {
        return true;
    }
    let generic_turn_terminal_state = window.key.family == StoryFamily::Turn
        && (combined.contains("turn completed") || combined.contains("turn failed"))
        && (headline.contains("completed ")
            || headline.contains("completed local")
            || headline.contains("completed repos")
            || deck.contains("turn completed")
            || deck.contains("turn failed"))
        && window
            .signals
            .latest_summary
            .as_deref()
            .is_none_or(|summary| !meaningful_summary(summary));
    if generic_turn_terminal_state {
        return true;
    }
    if window.signals.highest_score >= 90 {
        return false;
    }
    combined.contains("activity settled") || combined.contains("hit [project] command")
}

fn story_impact_rewrite(
    window: &StoryWindow,
    headline: &str,
    deck: &str,
) -> Option<(String, String)> {
    if window.key.family != StoryFamily::Turn {
        return None;
    }
    let combined = normalized_story_text(&format!(
        "{} {} {}",
        headline,
        deck,
        window.signals.latest_summary.as_deref().unwrap_or_default()
    ));
    let project = window.key.project_hash.as_deref().unwrap_or_default();
    let is_burn_p2p = combined.contains("burn p2p") || project.eq_ignore_ascii_case("burn_p2p");
    let is_burn_dragon =
        combined.contains("burn dragon") || project.eq_ignore_ascii_case("burn_dragon");
    let rewrite = if is_burn_p2p
        && combined.contains("release readiness")
        && combined.contains("green")
    {
        Some((
            "burn_p2p release readiness turns green",
            "only the remaining release lanes continue while the paired burn_dragon ci rerun stays active",
        ))
    } else if is_burn_p2p
        && (combined.contains("crate") || combined.contains("crates io"))
        && combined.contains("missing")
        && (combined.contains("burn dragon") || combined.contains("deploy"))
    {
        Some((
            "burn_p2p release gap blocks burn_dragon deploy",
            "the expected p2p crate version is not on crates.io yet, so the downstream deploy would fail at install time",
        ))
    } else if is_burn_p2p
        && combined.contains("publish preflight")
        && (combined.contains("dbus") || combined.contains("fmt"))
    {
        Some((
            "burn_p2p publish preflight is blocked locally",
            "cargo deny passed, but the fmt runner is still hitting the local rustup/dbus failure before publish can proceed",
        ))
    } else if is_burn_p2p
        && combined.contains("integration")
        && combined.contains("browser")
        && (combined.contains("green") || combined.contains("success"))
    {
        Some((
            "burn_p2p release checks narrow to final lanes",
            "browser, integration, and codeql checks are green while the last release lanes finish",
        ))
    } else if is_burn_p2p
        && combined.contains("published")
        && (combined.contains("green") || combined.contains("success"))
    {
        Some((
            "burn_p2p release publishes with checks green",
            "the package is live and downstream validation is moving through the paired burn_dragon lanes",
        ))
    } else if is_burn_p2p && combined.contains("publish completed successfully") {
        Some((
            "burn_p2p release publish completes",
            "the crate release cleared its publish path and downstream verification can continue",
        ))
    } else if is_burn_p2p && combined.contains("nightly") && combined.contains("completed green") {
        Some((
            "burn_p2p nightly validation finishes green",
            "the scheduled release-health lane cleared before the final repository sanity checks",
        ))
    } else if is_burn_p2p
        && (combined.contains("wasm compile") || combined.contains("wasm checks"))
        && (combined.contains("passed") || combined.contains("green"))
    {
        Some((
            "burn_p2p wasm browser checks pass",
            "the replacement receipt retry fix now clears the direct browser and app wasm checks",
        ))
    } else if is_burn_p2p
        && combined.contains("wasm browser clippy")
        && (combined.contains("clean") || combined.contains("passes"))
    {
        Some((
            "burn_p2p wasm browser lint is clean",
            "the local browser feature set now clears clippy against the release candidate",
        ))
    } else if is_burn_p2p
        && (combined.contains("fixed the check") || combined.contains("now passes"))
    {
        Some((
            "burn_p2p integration tooling checks pass",
            "the local xtask path now builds against the staged burn_p2p crates",
        ))
    } else if is_burn_dragon && combined.contains("head ci") && combined.contains("green") {
        Some((
            "burn_dragon ci turns green with wasm smoke",
            "the current head clears the browser smoke lane before final repository checks",
        ))
    } else if combined.contains("pages deploy")
        && combined.contains("browser canary")
        && (combined.contains("green") || combined.contains("completed successfully"))
    {
        Some((
            "burn_dragon deploy clears browser canaries",
            "the pages rollout and live browser checks completed on the current release candidate",
        ))
    } else if combined.contains("deployment diagnostics")
        && (combined.contains("passed") || combined.contains("green"))
        && (combined.contains("browser canary") || combined.contains("pages"))
    {
        Some((
            "burn_dragon deploy reaches browser canary",
            "edge diagnostics passed and the rollout advanced into pages plus live browser verification",
        ))
    } else if combined.contains("terraform apply")
        && (combined.contains("completed")
            || combined.contains("sync")
            || combined.contains("restart"))
    {
        Some((
            "burn_dragon edge rollout restarts after terraform",
            "infrastructure apply completed and the bootstrap runtime is syncing back to service readiness",
        ))
    } else if combined.contains("terraform")
        && (combined.contains("adopting") || combined.contains("state"))
        && combined.contains("deploy")
    {
        Some((
            "burn_dragon deploy enters terraform state adoption",
            "the edge rollout moved past binary compilation and is reconciling existing aws resources",
        ))
    } else if combined.contains("deploy compile finished")
        || (combined.contains("compile finished") && combined.contains("terraform"))
    {
        Some((
            "burn_dragon deploy leaves edge binary build",
            "the release path moved from native artifact compilation into infrastructure rollout",
        ))
    } else if combined.contains("edge binary")
        || ((combined.contains("binary compile") || combined.contains("binary build"))
            && (combined.contains("deploy") || combined.contains("rollout"))
            && !combined.contains("terraform"))
    {
        Some((
            "burn_dragon deploy builds edge binaries",
            "the production rollout is preparing native edge artifacts before infrastructure changes",
        ))
    } else if is_burn_dragon
        && (combined.contains("terraform apply") || combined.contains("infrastructure rollout"))
        && (combined.contains("deploy") || combined.contains("deployment"))
    {
        Some((
            "burn_dragon p2p deploy reaches terraform apply",
            "the edge rollout has moved past runner-side builds and is applying infrastructure changes",
        ))
    } else if is_burn_dragon
        && (combined.contains("edge binary")
            || combined.contains("binary build")
            || combined.contains("runner-side edge"))
        && (combined.contains("deploy") || combined.contains("deployment"))
    {
        Some((
            "burn_dragon p2p deploy is building edge binaries",
            "the production workflow is past ci dispatch and is preparing the edge artifacts for rollout",
        ))
    } else if is_burn_dragon
        && combined.contains("version surfaces")
        && combined.contains("line up")
    {
        Some((
            "burn_dragon release versions align with burn_p2p",
            "workspace and app manifests now point at the matching p2p release line",
        ))
    } else if is_burn_dragon
        && combined.contains("wasm")
        && (combined.contains("failed") || combined.contains("failure"))
    {
        Some((
            "burn_dragon wasm browser lane needs a ci fix",
            "the p2p browser check exposed a wasm clippy failure before release validation could finish",
        ))
    } else if is_burn_dragon && combined.contains("failing lint") {
        Some((
            "burn_dragon browser training lint is isolated",
            "the remaining failure is narrowed to the browser training contribution builder",
        ))
    } else if is_burn_dragon && combined.contains("rerun") && combined.contains("progress") {
        Some((
            "burn_dragon ci rerun tracks the wasm browser lane",
            "the deployment workflow retry is still active after the earlier browser training receipt failure",
        ))
    } else if combined.contains("github username")
        || combined.contains("username route")
        || (combined.contains("github") && combined.contains("discovery"))
    {
        Some((
            "github profile routes open verified public feed streams",
            "viewer urls resolve durable github identity before subscribing to visible stories",
        ))
    } else if combined.contains("publisher identity")
        || (combined.contains("verified") && combined.contains("publisher"))
    {
        Some((
            "remote feed headlines show verified publisher identity",
            "browser viewers can see the github account behind each feed story",
        ))
    } else if combined.contains("fabric") && combined.contains("subscription") {
        Some((
            "network discovery stays separate from feed subscriptions",
            "peers can support routing and browser handoff without auto-following feeds",
        ))
    } else if combined.contains("sign-in")
        || combined.contains("sign in")
        || combined.contains("oauth")
    {
        Some((
            "cli publishing now binds feeds to github sign-in",
            "native publishers can prove account identity before sending stories to the network",
        ))
    } else if combined.contains("headline image") || combined.contains("media layer") {
        Some((
            "feed stories support opt-in headline images",
            "publisher-side image generation stays disabled by default and runs behind guardrails",
        ))
    } else if combined.contains("deploy") || combined.contains("deployment") {
        Some((
            "feed deployment paths now exercise browser and edge delivery",
            "static pages, edge APIs, and network canaries can be verified together",
        ))
    } else if combined.contains("interactive feed")
        || combined.contains("automated projection")
        || combined.contains("timeline")
    {
        Some((
            "feed keeps projection automatic while adding browsable timelines",
            "interactive controls remain secondary to the hands-free broadcast view",
        ))
    } else if combined.contains("org-level")
        || combined.contains("org level")
        || combined.contains("organization")
    {
        Some((
            "organization-scoped feeds can gate p2p discovery",
            "teams can publish shared streams without opening every story to the public network",
        ))
    } else if (combined.contains("agent reel") || combined.contains("agent feed"))
        && (combined.contains("rename") || combined.contains("renamed"))
    {
        Some((
            "agent_feed naming is consistent across crates and publish surfaces",
            "the workspace, binaries, and public packages now use one product name",
        ))
    } else if combined.contains("real codex stream")
        || combined.contains("settled summarization")
        || combined.contains("p2p capsule")
        || combined.contains("story capsule")
    {
        Some((
            "feed turns live agent work into safer public story capsules",
            "local activity is aggregated into settled, redacted headlines before it reaches the network",
        ))
    } else if combined.contains("summarization pass")
        || combined.contains("story-quality")
        || combined.contains("story quality")
        || combined.contains("publish gating")
    {
        Some((
            "feed suppresses duplicate mechanics before publishing",
            "the story compiler now favors outcome changes over command, file, and test chatter",
        ))
    } else if combined.contains("error logging") {
        Some((
            "feed surfaces capture and network failures more clearly",
            "operators get structured logs when local or p2p story delivery misbehaves",
        ))
    } else if combined.contains("diloco") {
        Some((
            "burn_p2p gets a focused diloco training slice",
            "the distributed training path moves toward practical multi-peer optimizer coverage",
        ))
    } else if combined.contains("actions state is green")
        || combined.contains("ci, aws deploy")
        || combined.contains("pages deploy")
    {
        Some((
            "ci and deployment checks are green for the feed release path",
            "browser pages, aws edge deployment, and repository checks completed together",
        ))
    } else {
        None
    }?;

    Some((rewrite.0.to_string(), format!("{}.", rewrite.1)))
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
        Some(EventKind::AgentMessage) => Some("progress update"),
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

fn command_topic(command: &str) -> Option<&'static str> {
    let lowered = command.to_ascii_lowercase();
    let first = lowered.split_whitespace().next().unwrap_or_default();
    if first.is_empty() {
        return None;
    }
    if lowered.contains("gh run")
        || lowered.contains("github")
        || lowered.contains("mcp github")
        || lowered.contains("workflow")
        || lowered.contains("ci")
    {
        return Some("ci status");
    }
    if first == "cargo"
        || first == "just"
        || first == "pytest"
        || first == "npm"
        || first == "pnpm"
        || first == "yarn"
        || lowered.contains(" test")
    {
        return Some("verification");
    }
    if first == "rg"
        || first == "sed"
        || first == "nl"
        || first == "cat"
        || first == "find"
        || lowered.contains("grep")
    {
        return Some("code inspection");
    }
    if first == "git" {
        return Some("repository state");
    }
    if first == "curl" || first == "wget" {
        return Some("remote service check");
    }
    Some("shell checks")
}

fn has_command_burst(window: &StoryWindow) -> bool {
    window.key.family == StoryFamily::Turn
        && window.counters.commands >= 4
        && window.counters.files_changed == 0
        && window.counters.tests_failed == 0
        && window.counters.tests_passed == 0
        && window.counters.permissions == 0
        && window.counters.mcp_failures == 0
}

fn command_burst_label(window: &StoryWindow) -> Option<&'static str> {
    if !has_command_burst(window) {
        return None;
    }
    if has_command_topic(window, "ci status") {
        return Some("checked ci status");
    }
    if has_command_topic(window, "verification") {
        return Some("ran verification checks");
    }
    if has_command_topic(window, "code inspection") {
        return Some("inspected code paths");
    }
    if has_command_topic(window, "repository state") {
        return Some("checked repository state");
    }
    if has_command_topic(window, "remote service check") {
        return Some("checked remote services");
    }
    Some("worked through shell checks")
}

fn command_burst_deck(window: &StoryWindow) -> Option<String> {
    let scope = command_burst_scope(window)?;
    Some(format!(
        "{} safe command events settled around {}",
        window.counters.commands.min(99),
        scope
    ))
}

fn has_command_topic(window: &StoryWindow, topic: &str) -> bool {
    window
        .signals
        .command_topics
        .iter()
        .any(|existing| existing == topic)
}

fn command_burst_scope(window: &StoryWindow) -> Option<&'static str> {
    if !has_command_burst(window) {
        return None;
    }
    if has_command_topic(window, "ci status") {
        return Some("ci status");
    }
    if has_command_topic(window, "verification") {
        return Some("verification");
    }
    if has_command_topic(window, "code inspection") {
        return Some("code inspection");
    }
    if has_command_topic(window, "repository state") {
        return Some("repository state");
    }
    if has_command_topic(window, "remote service check") {
        return Some("remote service checks");
    }
    Some("shell checks")
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

fn normalized_story_text(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch.is_whitespace() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn semantic_story_fingerprint(headline: &str, deck: &str) -> String {
    let signature = story_update_signature(headline, deck);
    format!(
        "topic={} state={} impact={}",
        signature.topic_fingerprint, signature.state_fingerprint, signature.impact_fingerprint
    )
}

fn active_update_fingerprint(window: &StoryWindow, story: &CompiledStory) -> String {
    if is_activity_checkpoint_story(story) {
        return semantic_story_fingerprint(&story.headline, &story.deck);
    }
    let summary_signature = window
        .signals
        .latest_summary
        .as_deref()
        .map(|summary| story_update_signature(summary, ""));
    let mut fingerprint = semantic_story_fingerprint(&story.headline, &story.deck);
    if let Some(signature) = summary_signature {
        fingerprint.push_str(" source_state=");
        fingerprint.push_str(&signature.state_fingerprint);
        fingerprint.push_str(" source_impact=");
        fingerprint.push_str(&signature.impact_fingerprint);
    }
    fingerprint
}

fn is_activity_checkpoint_story(story: &CompiledStory) -> bool {
    let deck = story.deck.to_ascii_lowercase();
    deck.contains("open turn")
        && (deck.contains("final outcome")
            || deck.contains("final handoff")
            || deck.contains("final result")
            || deck.contains("final pass/fail"))
}

#[must_use]
pub fn story_update_signature(headline: &str, deck: &str) -> StoryUpdateSignature {
    let normalized = normalized_story_text(&format!("{headline} {deck}"));
    let mut topic = BTreeSet::new();
    let mut state = BTreeSet::new();
    let mut impact = BTreeSet::new();

    for word in normalized.split_whitespace() {
        if let Some(label) = state_label(word) {
            state.insert(label.to_string());
            continue;
        }
        if let Some(label) = impact_label(word) {
            impact.insert(label.to_string());
        }
        if !is_story_signature_stopword(word)
            && state_label(word).is_none()
            && !is_story_update_verb(word)
        {
            topic.insert(stem_story_term(word));
        }
    }

    StoryUpdateSignature {
        topic_fingerprint: topic.into_iter().take(14).collect::<Vec<_>>().join(":"),
        state_fingerprint: state.into_iter().take(8).collect::<Vec<_>>().join(":"),
        impact_fingerprint: impact.into_iter().take(8).collect::<Vec<_>>().join(":"),
    }
}

#[must_use]
pub fn story_update_relation(
    candidate: &StoryUpdateSignature,
    previous: &StoryUpdateSignature,
) -> StoryUpdateRelation {
    if candidate == previous {
        return StoryUpdateRelation::ExactRepeat;
    }
    if !same_story_topic(&candidate.topic_fingerprint, &previous.topic_fingerprint) {
        return StoryUpdateRelation::NewTopic;
    }
    if candidate.state_fingerprint != previous.state_fingerprint {
        return StoryUpdateRelation::StateChanged;
    }
    if candidate.impact_fingerprint != previous.impact_fingerprint {
        return StoryUpdateRelation::ImpactChanged;
    }
    StoryUpdateRelation::SameState
}

fn same_story_topic(left: &str, right: &str) -> bool {
    if left.is_empty() || right.is_empty() {
        return left == right;
    }
    if left == right {
        return true;
    }
    let left = left.split(':').collect::<BTreeSet<_>>();
    let right = right.split(':').collect::<BTreeSet<_>>();
    let intersection = left.intersection(&right).count();
    let union = left.union(&right).count().max(1);
    (intersection * 100) / union >= 60
}

fn state_label(word: &str) -> Option<&'static str> {
    match word {
        "blocked" | "blocker" | "blocking" | "stuck" | "unavailable" | "degraded" | "down" => {
            Some("blocked")
        }
        "broken" | "fail" | "failed" | "failing" | "failure" | "red" | "regressed"
        | "regression" => Some("failing"),
        "fixed" | "fixes" | "resolved" | "restored" | "recovered" | "cleared" => Some("fixed"),
        "green" | "pass" | "passed" | "passing" | "healthy" => Some("passing"),
        "live" | "available" | "connected" | "online" => Some("available"),
        "deploy" | "deployed" | "deployment" | "published" | "released" | "shipped" | "launch"
        | "launched" => Some("shipped"),
        "implemented" | "added" | "enabled" | "supports" | "support" => Some("implemented"),
        "verified" | "validated" | "confirmed" | "proven" => Some("verified"),
        "planned" | "designed" | "drafted" => Some("planned"),
        "denied" | "rejected" => Some("denied"),
        "requested" | "pending" | "waiting" => Some("requested"),
        _ => None,
    }
}

fn impact_label(word: &str) -> Option<&'static str> {
    match word {
        "auth" | "oauth" | "callback" | "signin" | "login" => Some("auth"),
        "browser" | "page" | "pages" | "ui" | "ux" | "mobile" | "desktop" => Some("browser"),
        "capture" | "codex" | "claude" | "transcript" | "session" => Some("capture"),
        "deploy" | "deployment" | "edge" | "terraform" | "aws" => Some("deploy"),
        "discovery" | "feed" | "feeds" | "follower" | "following" | "network" | "p2p" | "peer"
        | "peers" | "subscription" | "subscriptions" => Some("network"),
        "install" | "package" | "publish" | "release" | "cargo" => Some("release"),
        "privacy" | "redaction" | "guardrail" | "secret" | "secrets" => Some("safety"),
        "summary" | "summaries" | "summarization" | "headline" | "story" | "stories" => {
            Some("summary")
        }
        "test" | "tests" | "ci" | "check" | "checks" | "canary" => Some("verification"),
        _ => None,
    }
}

fn is_story_signature_stopword(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "by"
            | "codex"
            | "claude"
            | "for"
            | "from"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "the"
            | "to"
            | "with"
            | "now"
            | "again"
            | "after"
            | "before"
            | "latest"
            | "new"
            | "one"
            | "two"
            | "three"
            | "four"
            | "five"
            | "six"
            | "seven"
            | "eight"
            | "nine"
            | "ten"
    ) || word.chars().all(|ch| ch.is_ascii_digit())
}

fn is_story_update_verb(word: &str) -> bool {
    matches!(
        word,
        "advance"
            | "advanced"
            | "advances"
            | "become"
            | "became"
            | "becomes"
            | "change"
            | "changed"
            | "changes"
            | "finish"
            | "finished"
            | "finishes"
            | "move"
            | "moved"
            | "moves"
            | "shift"
            | "shifted"
            | "shifts"
            | "stay"
            | "stayed"
            | "stays"
            | "turn"
            | "turned"
            | "turns"
            | "update"
            | "updated"
            | "updates"
    )
}

fn stem_story_term(word: &str) -> String {
    word.strip_suffix('s')
        .filter(|stem| stem.len() > 3)
        .unwrap_or(word)
        .to_string()
}

fn summary_is_redundant(input: &str) -> bool {
    let lowered = input.to_ascii_lowercase();
    let trimmed = lowered.trim_start();
    if lowered.contains("/home/") || lowered.contains("\\home\\") {
        return true;
    }
    if trimmed.starts_with('{')
        && (trimmed.contains("\"headline\"")
            || trimmed.contains("\"deck\"")
            || trimmed.contains("\"publish\"")
            || trimmed.contains("\"memory_digest\"")
            || trimmed.contains("\"semantic_fingerprint\""))
    {
        return true;
    }
    if trimmed.starts_with("status ")
        || trimmed.starts_with("exit ")
        || trimmed.starts_with("turn completed in ")
        || trimmed.starts_with("done")
        || trimmed.starts_with("i'm ")
        || trimmed.starts_with("i’m ")
        || trimmed.starts_with("i'll ")
        || trimmed.starts_with("i’ll ")
        || trimmed.starts_with("implemented and pushed")
        || trimmed.starts_with("model ")
        || trimmed.starts_with("no,")
        || trimmed.starts_with("no.")
        || trimmed.starts_with("not fully")
        || trimmed.starts_with("implemented and published")
        || trimmed.starts_with("the code and crates are published")
        || trimmed.starts_with("the commit is created")
        || trimmed.starts_with("the current daemon")
        || trimmed.starts_with("with that command")
        || trimmed.starts_with("with the currently")
    {
        return true;
    }
    [
        "agent message recorded",
        "browser peer/ui seems broken",
        "changed files",
        "checks ci status",
        "ci status",
        "cli check",
        "command events",
        "command lifecycle captured",
        "confirms pass state",
        "patch activity captured",
        "raw diff omitted",
        "raw diff",
        "raw output omitted",
        "raw transcript omitted",
        "raw content omitted",
        "plan updated",
        "plan update recorded",
        "planning state advanced",
        "question answered",
        "dry-run mode",
        "focused rust tests",
        "i'm committing",
        "i’m committing",
        "implemented and pushed",
        "local capture is alive",
        "local publisher",
        "one more important nuance",
        "pushing it now",
        "remote fast-forward",
        "repository state",
        "run state",
        "safe command",
        "seems broken",
        "settles run state",
        "shifts feed to edits",
        "shell check",
        "shell command failed",
        "targeted tests",
        "there are no push-triggered workflows",
        "there's a second problem",
        "there s a second problem",
        "this session is network-disabled",
        "transcript sample",
        "test command failed",
        "test command passed",
        "the key live check passed",
        "without raw content",
        "without command output",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn meaningful_summary(input: &str) -> bool {
    !summary_is_redundant(input) && summary_has_work_context(input)
}

fn active_update_summary(input: &str) -> bool {
    if summary_is_redundant(input) {
        return false;
    }
    let normalized = normalized_story_text(input);
    if normalized.is_empty()
        || normalized.starts_with("i am ")
        || normalized.starts_with("i m ")
        || normalized.starts_with("i ll ")
        || normalized.starts_with("i will ")
        || normalized.contains("waiting for the build")
        || normalized.contains("polling at")
        || summary_is_operator_chatter(&normalized)
    {
        return false;
    }
    (summary_has_work_context(input) || summary_mentions_public_project(&normalized))
        && active_summary_has_release_outcome(input)
}

fn active_progress_summary(input: &str) -> bool {
    if summary_is_redundant(input) {
        return false;
    }
    let normalized = normalized_story_text(input);
    if normalized.len() < 80
        || normalized.starts_with("i am ")
        || normalized.starts_with("i m ")
        || normalized.starts_with("i ll ")
        || normalized.starts_with("i will ")
        || summary_is_operator_chatter(&normalized)
    {
        return false;
    }
    if !(summary_has_work_context(input) || summary_mentions_public_project(&normalized)) {
        return false;
    }
    [
        "adds",
        "aligns",
        "builds",
        "connects",
        "covers",
        "enables",
        "exposes",
        "extends",
        "hardens",
        "improves",
        "integrates",
        "moves",
        "opens",
        "prepares",
        "protects",
        "reduces",
        "removes",
        "resolves",
        "supports",
        "tracks",
        "validates",
        "verifies",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn summary_is_operator_chatter(normalized: &str) -> bool {
    [
        "i am checking",
        "i m checking",
        "i'm checking",
        "i’m checking",
        "i am reading",
        "i m reading",
        "i'm reading",
        "i’m reading",
        "i ll focus",
        "i'll focus",
        "i’ll focus",
        "i will focus",
        "i ll check",
        "i'll check",
        "i’ll check",
        "i will check",
        "i am tightening",
        "i m tightening",
        "i'm tightening",
        "i’m tightening",
        "i am patching",
        "i m patching",
        "i'm patching",
        "i’m patching",
        "i am waiting",
        "i m waiting",
        "i'm waiting",
        "i’m waiting",
        "i found the",
        "i m past",
        "i'm past",
        "i’m past",
        "your feed",
        "your workstation feed",
        "your active codex",
        "the live edge now has",
        "public edge now shows",
        "edge now shows",
        "the live state confirms",
        "the latest evidence is more specific",
        "the edge path accepted",
        "the daemon is now actually",
        "the current daemon",
        "the daemon itself",
        "the fixed daemon is running",
        "the new binary started",
        "the repo is on the pushed",
        "amended commit",
        "ci for the amended commit",
        "pushed commit",
        "background launch",
        "background start",
        "foreground run",
        "same serve command under",
        "short timeout",
        "systemd",
        "guardrail leak",
        "covered by a new test",
        "running service has both fixes",
        "both fixes",
        "installing once more",
        "reinstalling once more",
        "source build",
        "hot paths",
        "current publisher is stuck",
        "story gate",
        "startup context",
        "edge snapshot request",
        "public snapshot",
        "public edge capsule",
        "public publish loop",
        "publish worker",
        "edge call",
        "debugging status",
        "concrete regression",
        "operator narration",
        "public feed and hides the real signal",
        "recent_summaries",
        "restart-safe duplicate memory",
        "stale native-smoke update",
        "future new work",
        "behavior we wanted",
        "correctly skipped",
        "this debugging session",
        "the bug is not no capture",
        "startup lines",
        "dead daemon",
        "existing 7777 daemon",
        "publish interval",
        "listening on 127.0.0.1",
        "127.0.0.1:7777",
        "the fixed binary is installed",
        "restart the local daemon",
        "live daemon capture",
        "the local story",
        "after the restart",
        "correctly declined",
        "full workspace test pass",
        "runtime check",
        "installed binary on path",
        "stability bug",
        "user-facing story",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn active_summary_has_release_outcome(input: &str) -> bool {
    let normalized = normalized_story_text(input);
    [
        "accepted",
        "blocked",
        "broken",
        "complete",
        "completed",
        "degraded",
        "deploy",
        "deployed",
        "deployment",
        "failed",
        "failure",
        "fixed",
        "green",
        "landed",
        "missing",
        "passed",
        "preflight",
        "publish",
        "published",
        "publishes",
        "pushed",
        "ready",
        "readiness",
        "regressed",
        "reliably",
        "released",
        "rerun",
        "rollout",
        "shipped",
        "success",
        "turned green",
        "unblocked",
        "works",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn activity_checkpoint_copy(window: &StoryWindow) -> Option<(String, String)> {
    if window.key.family != StoryFamily::Turn {
        return None;
    }
    let project = concrete_activity_project(window)?;
    let min_publishable_events = if project_is_public_signal(&project) {
        4
    } else {
        5
    };
    if window.publishable_events < min_publishable_events {
        return None;
    }
    if window.counters.tests_failed > 0 {
        return Some((
            format!("{project} verification is red"),
            "a failing test signal arrived while the turn is still open; final recovery has not landed yet."
                .to_string(),
        ));
    }
    if window.counters.tests_passed > 0 && window.counters.files_changed > 0 {
        return Some((
            format!("{project} changes are passing verification"),
            "edits and passing checks are accumulating in the open turn before the final outcome is reported."
                .to_string(),
        ));
    }
    if window.counters.tests_passed > 0 {
        return Some((
            format!("{project} validation is passing"),
            "test signals are green in the open turn; the final handoff has not been reported yet."
                .to_string(),
        ));
    }
    if window.counters.files_changed > 0
        && (has_command_topic(window, "verification") || has_command_topic(window, "ci status"))
    {
        return Some((
            format!("{project} update is moving through verification"),
            "changed files are being checked in the open turn before a final pass/fail result is available."
                .to_string(),
        ));
    }
    if window.counters.commands >= min_publishable_events && has_command_topic(window, "ci status")
    {
        return Some((
            format!("{project} release checks are being monitored"),
            "workflow state is being checked from an open turn; no new final pass/fail outcome has landed yet."
                .to_string(),
        ));
    }
    if window.counters.commands >= min_publishable_events
        && has_command_topic(window, "verification")
    {
        return Some((
            format!("{project} verification is active"),
            "build or test commands are running in the open turn; the final result has not been reported yet."
                .to_string(),
        ));
    }
    if window.counters.commands >= min_publishable_events
        && (has_command_topic(window, "remote service check")
            || has_command_topic(window, "repository state"))
        && project_is_public_signal(&project)
    {
        return Some((
            format!("{project} release state is being checked"),
            "repository or remote service checks are active in the open turn before a final outcome is available."
                .to_string(),
        ));
    }
    None
}

fn concrete_activity_project(window: &StoryWindow) -> Option<String> {
    let project = window.key.project_hash.as_deref()?;
    if matches!(
        project,
        "repo" | "repos" | "workspace" | "workspaces" | "local"
    ) {
        return None;
    }
    if project_is_internal_debug_project(project)
        && window.counters.tests_failed == 0
        && window.counters.tests_passed == 0
        && window.counters.files_changed == 0
    {
        return None;
    }
    Some(project.to_string())
}

fn project_is_internal_debug_project(project: &str) -> bool {
    matches!(project, "agent_reel" | "agent_feed")
}

fn project_is_public_signal(project: &str) -> bool {
    matches!(project, "burn_p2p" | "burn_dragon")
}

fn summary_has_work_context(input: &str) -> bool {
    let lowered = input.to_ascii_lowercase();
    [
        "auth",
        "avatar",
        "aws",
        "browser",
        "broadcast",
        "callback",
        "capture",
        "canary",
        "cli",
        "ci",
        "codeql",
        "coverage",
        "deployment",
        "discovery",
        "diloco",
        "edge",
        "error",
        "feed",
        "github",
        "guardrail",
        "identity",
        "install",
        "integration",
        "logging",
        "mcp",
        "model",
        "network",
        "open source",
        "package",
        "peer",
        "p2p",
        "privacy",
        "publish",
        "release",
        "readiness",
        "route",
        "security",
        "story",
        "stream",
        "subscription",
        "summarization",
        "terraform",
        "training",
        "wasm",
        "webgpu",
        "workflow",
        "user",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
        || summary_mentions_public_project(&lowered)
}

fn summary_mentions_public_project(normalized_or_lowered: &str) -> bool {
    [
        "agent_feed",
        "agent reel",
        "agent_reel",
        "burn dragon",
        "burn p2p",
        "burn_dragon",
        "burn_p2p",
    ]
    .iter()
    .any(|needle| normalized_or_lowered.contains(needle))
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
    fn meaningful_agent_message_publishes_after_flush() {
        let mut compiler = StoryCompiler::default();
        let mut message = event(EventKind::AgentMessage, "codex posted an update");
        message.summary =
            Some("Burn_p2p browser training receipts now flush reliably.".to_string());

        assert!(compiler.ingest(message).is_empty());
        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0].family, StoryFamily::Turn);
        assert!(
            stories[0]
                .headline
                .contains("burn_p2p browser training receipts")
        );
        assert!(stories[0].score >= DEFAULT_MIN_SCORE);
    }

    #[test]
    fn active_progress_message_can_publish_without_turn_completion() {
        let mut compiler = StoryCompiler::default();
        let mut message = event(EventKind::AgentMessage, "codex posted an update");
        message.project = Some("burn_p2p".to_string());
        message.summary = Some(
            "Burn_p2p optimizer coverage extends the DiLoCo training path for multi-peer validation across native and browser workers."
                .to_string(),
        );

        assert!(compiler.ingest(message).is_empty());
        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0].project.as_deref(), Some("burn_p2p"));
        assert!(
            stories[0]
                .headline
                .to_ascii_lowercase()
                .contains("burn_p2p")
        );
    }

    #[test]
    fn operator_chatter_does_not_publish_as_story() {
        for summary in [
            "The live edge now has five accepted headlines from your workstation feed. I’m tightening the final story gate.",
            "The new binary started and then exited without writing an error after the startup lines. That is a separate stability bug in the publish daemon.",
            "The full workspace test pass is green. I’m doing one last runtime check now: installed binary on PATH, live daemon.",
            "The local story after the restart is now from the active `burn_dragon` session, and the p2p publisher correctly declined to republish it.",
            "The repo is on the pushed `4bf38b6` revision. I’m reading the two hot paths now: the p2p publish loop and the open-turn story compiler.",
            "The live state confirms the bug is not “no capture”: local events and one newly published local bulletin exist. The current publisher is stuck in publishing.",
            "The current daemon is running the latest source install and it is publishing, but the story gate is still starving most work.",
            "There’s a second problem now: the local publisher queued a burn_dragon story, but the edge snapshot request returned 504.",
            "I found a concrete regression while validating: my own debugging/status messages are being treated as publishable agent work.",
            "The key live check passed: the publisher now says `recent_summaries=8` on startup and the summarizer skipped the stale native-smoke update.",
            "The background start exited unexpectedly without an error line. I’m going to run the same serve command under a short timeout.",
            "The daemon itself stayed healthy under an 8s foreground run and started importing the burn_p2p/burn_dragon root-workspace session immediately.",
            "The guardrail leak is fixed and covered by a new test. I’m reinstalling once more so the running service has both fixes.",
            "Public edge now shows the new capsule at `17:52:55Z`, and CI for the amended commit is green. I’m waiting for the Pages and AWS deploy workflows from the same commit to finish.",
            "agent_reel now surfaces a new public edge capsule; the amended commit reports green ci, with rollout movement still awaiting the next operator-facing step.",
        ] {
            let mut compiler = StoryCompiler::default();
            let mut message = event(EventKind::AgentMessage, "codex posted an update");
            message.summary = Some(summary.to_string());

            assert!(compiler.ingest(message).is_empty());
            assert!(compiler.flush().is_empty());
            let decision = compiler
                .diagnostics()
                .last_decision
                .expect("decision recorded");
            assert!(
                matches!(
                    decision.action,
                    StoryDecisionAction::Waiting | StoryDecisionAction::Retained
                ),
                "operator chatter should remain unpublished"
            );
        }
    }

    #[test]
    fn same_turn_can_publish_meaningfully_changed_updates() {
        let mut compiler = StoryCompiler::default();
        let mut first = event(EventKind::AgentMessage, "codex posted an update");
        first.summary = Some("Burn_p2p browser training receipts now flush reliably.".to_string());
        assert!(compiler.ingest(first).is_empty());
        assert_eq!(compiler.flush().len(), 1);

        let mut second = event(EventKind::AgentMessage, "codex posted an update");
        second.summary =
            Some("Burn_dragon WebGPU training publishes browser receipts again.".to_string());
        assert!(compiler.ingest(second).is_empty());
        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert!(
            stories[0]
                .headline
                .contains("burn_dragon WebGPU training publishes")
        );
    }

    #[test]
    fn same_turn_repeated_update_is_deduped_semantically() {
        let mut compiler = StoryCompiler::default();
        let mut first = event(EventKind::AgentMessage, "codex posted an update");
        first.summary = Some("Burn_p2p browser training receipts now flush reliably.".to_string());
        assert!(compiler.ingest(first).is_empty());
        assert_eq!(compiler.flush().len(), 1);

        let mut repeat = event(EventKind::AgentMessage, "codex posted an update");
        repeat.summary = Some("Burn_p2p browser training receipts now flush reliably.".to_string());
        assert!(compiler.ingest(repeat).is_empty());

        assert!(compiler.flush().is_empty());
        assert_eq!(
            compiler
                .diagnostics()
                .last_decision
                .as_ref()
                .map(|decision| decision.action),
            Some(StoryDecisionAction::Deduped)
        );
    }

    #[test]
    fn same_topic_status_progression_publishes_again() {
        let mut compiler = StoryCompiler::default();
        let mut blocked = event(EventKind::AgentMessage, "codex posted an update");
        blocked.project = Some("burn_p2p".to_string());
        blocked.summary =
            Some("Burn_p2p release checks are blocked by a failing browser canary.".to_string());
        assert!(compiler.ingest(blocked).is_empty());
        assert_eq!(compiler.flush().len(), 1);

        let mut fixed = event(EventKind::AgentMessage, "codex posted an update");
        fixed.project = Some("burn_p2p".to_string());
        fixed.summary =
            Some("Burn_p2p release checks are green after the browser canary passed.".to_string());
        assert!(compiler.ingest(fixed).is_empty());
        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert!(stories[0].headline.contains("burn_p2p release checks"));
        assert!(
            stories[0].deck.contains("green") || stories[0].headline.contains("green"),
            "changed status should survive active-update dedupe"
        );
    }

    #[test]
    fn active_ci_outcome_publishes_before_turn_completion() {
        let mut compiler = StoryCompiler::default();
        let mut command = event(EventKind::CommandExec, "codex started a command");
        command.project = Some("burn_p2p".to_string());
        command.command = Some(
            "sleep 240; gh run list --repo aberration-technology/burn_p2p --branch main"
                .to_string(),
        );
        assert!(compiler.ingest(command).is_empty());

        let mut update = event(EventKind::AgentMessage, "codex posted an update");
        update.project = Some("burn_p2p".to_string());
        update.summary = Some(
            "`burn_p2p` Release Readiness is green too. Only `burn_p2p` PR Fast and the rerun `burn_dragon` CI remain active.".to_string(),
        );
        assert!(compiler.ingest(update).is_empty());

        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0].project.as_deref(), Some("burn_p2p"));
        assert_eq!(
            stories[0].headline,
            "burn_p2p release readiness turns green"
        );
        assert!(stories[0].deck.contains("remaining release lanes"));
        assert!(stories[0].score >= 72);
        assert!(stories[0].context_score >= 76);
    }

    #[test]
    fn active_ci_update_is_not_repeated_on_next_flush() {
        let mut compiler = StoryCompiler::default();
        let mut update = event(EventKind::AgentMessage, "codex posted an update");
        update.project = Some("burn_p2p".to_string());
        update.summary = Some(
            "`burn_p2p` Integration is green now; Browser and CodeQL are also green. Remaining there: PR Fast and Release Readiness.".to_string(),
        );
        assert!(compiler.ingest(update).is_empty());

        assert_eq!(compiler.flush().len(), 1);
        assert!(compiler.flush().is_empty());
        assert_eq!(
            compiler
                .diagnostics()
                .last_decision
                .as_ref()
                .map(|decision| decision.action),
            Some(StoryDecisionAction::Deduped)
        );
    }

    #[test]
    fn active_publish_status_rewrites_to_news_headline() {
        let mut compiler = StoryCompiler::default();
        let mut update = event(EventKind::AgentMessage, "codex posted an update");
        update.project = Some("burn_p2p".to_string());
        update.summary = Some(
            "The `burn_p2p` side is already green and published as `0.21.0-pre.38`; `burn_dragon` lint passed with the new version. The remaining waits are release lanes.".to_string(),
        );
        assert!(compiler.ingest(update).is_empty());

        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert_eq!(
            stories[0].headline,
            "burn_p2p release publishes with checks green"
        );
        assert!(!stories[0].deck.contains("I’m"));
    }

    #[test]
    fn active_missing_crate_update_rewrites_to_news_headline() {
        let mut compiler = StoryCompiler::default();
        let mut update = event(EventKind::AgentMessage, "codex posted an update");
        update.project = Some("burn_p2p".to_string());
        update.summary = Some(
            "Crates.io currently reports `burn_p2p_bootstrap` max version `0.21.0-pre.42`; `0.21.0-pre.43` is missing. Since burn_dragon’s production workflow references `0.21.0-pre.43`, I’m going to publish the burn_p2p release now rather than letting the deploy fail at crate install time.".to_string(),
        );
        assert!(compiler.ingest(update).is_empty());

        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert_eq!(
            stories[0].headline,
            "burn_p2p release gap blocks burn_dragon deploy"
        );
        assert!(stories[0].deck.contains("crates.io"));
        assert!(!stories[0].deck.contains("I’m"));
    }

    #[test]
    fn active_publish_preflight_failure_rewrites_to_news_headline() {
        let mut compiler = StoryCompiler::default();
        let mut update = event(EventKind::AgentMessage, "codex posted an update");
        update.project = Some("burn_p2p".to_string());
        update.summary = Some(
            "The publish preflight passed `cargo deny` with the `/tmp` Cargo home, then hit the same local rustup/DBus problem on `cargo fmt --check`. I’m going to run the preflight with toolchain binaries pinned in the environment so `fmt` and later `clippy` don’t route through rustup shims.".to_string(),
        );
        assert!(compiler.ingest(update).is_empty());

        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert_eq!(
            stories[0].headline,
            "burn_p2p publish preflight is blocked locally"
        );
        assert!(stories[0].deck.contains("cargo deny passed"));
        assert!(!stories[0].deck.contains("I’m"));
    }

    #[test]
    fn active_burn_dragon_ci_status_rewrites_to_news_headline() {
        let mut compiler = StoryCompiler::default();
        let mut update = event(EventKind::AgentMessage, "codex posted an update");
        update.project = Some("burn_dragon".to_string());
        update.summary = Some(
            "The current `burn_dragon` head CI is now green, including `wasm-smoke`. I’m doing the last repository and workflow status checks now.".to_string(),
        );
        assert!(compiler.ingest(update).is_empty());

        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert_eq!(
            stories[0].headline,
            "burn_dragon ci turns green with wasm smoke"
        );
        assert!(stories[0].deck.contains("browser smoke lane"));
        assert!(!stories[0].deck.contains("I’m"));
    }

    #[test]
    fn same_open_turn_deploy_stage_changes_publish_again() {
        let mut compiler = StoryCompiler::default();
        let mut terraform = event(EventKind::AgentMessage, "codex posted an update");
        terraform.project = Some("burn_p2p".to_string());
        terraform.summary = Some(
            "The deploy compile finished and Terraform is running; current step is adopting existing AWS resources into state. CI test is still running."
                .to_string(),
        );
        assert!(compiler.ingest(terraform).is_empty());
        let first = compiler.flush();
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].headline,
            "burn_dragon deploy enters terraform state adoption"
        );

        let mut canary = event(EventKind::AgentMessage, "codex posted an update");
        canary.project = Some("burn_p2p".to_string());
        canary.summary = Some(
            "Pages deploy and every live browser canary lane are green on `5898cfc`, including the chromium-webrtc-direct-checkpoint lane that failed before."
                .to_string(),
        );
        assert!(compiler.ingest(canary).is_empty());
        let second = compiler.flush();

        assert_eq!(second.len(), 1);
        assert_eq!(
            second[0].headline,
            "burn_dragon deploy clears browser canaries"
        );
    }

    #[test]
    fn same_open_turn_wording_change_without_state_change_is_deduped() {
        let mut compiler = StoryCompiler::default();
        let mut first = event(EventKind::AgentMessage, "codex posted an update");
        first.project = Some("agent_reel".to_string());
        first.summary = Some(
            "Static pages, edge APIs, and network canaries can be verified together across the feed deployment path."
                .to_string(),
        );
        assert!(compiler.ingest(first).is_empty());
        assert_eq!(compiler.flush().len(), 1);

        let mut repeated = event(EventKind::AgentMessage, "codex posted an update");
        repeated.project = Some("agent_reel".to_string());
        repeated.summary = Some(
            "Static pages, edge APIs, and network canaries can still be verified together across the same feed deployment path."
                .to_string(),
        );
        assert!(compiler.ingest(repeated).is_empty());

        assert!(compiler.flush().is_empty());
        assert_eq!(
            compiler
                .diagnostics()
                .last_decision
                .as_ref()
                .map(|decision| decision.action),
            Some(StoryDecisionAction::Deduped)
        );
    }

    #[test]
    fn active_burn_dragon_deploy_stage_publishes_project_specific_headline() {
        let mut compiler = StoryCompiler::default();
        let mut update = event(EventKind::AgentMessage, "codex posted an update");
        update.project = Some("burn_dragon".to_string());
        update.summary = Some(
            "The deploy has moved past binary builds and is now in terraform apply.".to_string(),
        );
        assert!(compiler.ingest(update).is_empty());

        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert_eq!(
            stories[0].headline,
            "burn_dragon p2p deploy reaches terraform apply"
        );
        assert!(stories[0].deck.contains("edge rollout"));
        assert!(!stories[0].headline.contains("feed deployment paths"));
    }

    #[test]
    fn file_change_rolls_up_without_standalone_story() {
        let mut compiler = StoryCompiler::default();
        let mut start = event(EventKind::CommandExec, "codex started a command");
        start.command = Some("cargo test --all".to_string());
        assert!(compiler.ingest(start).is_empty());

        let mut changed = event(EventKind::FileChanged, "codex patch applied");
        changed.files = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
        changed.summary = Some("2 changed files. raw diff omitted.".to_string());
        changed.score_hint = Some(82);
        let stories = compiler.ingest(changed);

        assert!(stories.is_empty());
    }

    #[test]
    fn turn_completion_rolls_up_prior_context() {
        let mut compiler = StoryCompiler::default();
        let mut start = event(EventKind::CommandExec, "codex started a command");
        start.command = Some("cargo test --all".to_string());
        assert!(compiler.ingest(start).is_empty());

        let mut changed = event(EventKind::FileChanged, "codex patch applied");
        changed.files = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
        changed.summary = Some("2 changed files. raw diff omitted.".to_string());
        changed.score_hint = Some(82);
        assert!(compiler.ingest(changed).is_empty());

        let mut pass = event(EventKind::TestPass, "codex tests passed");
        pass.command = Some("cargo test --all".to_string());
        pass.summary = Some("test command passed.".to_string());
        pass.score_hint = Some(76);
        assert!(compiler.ingest(pass).is_empty());

        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some("Implemented the release feed capture path.".to_string());
        complete.score_hint = Some(82);
        let stories = compiler.ingest(complete);

        let turn = stories
            .iter()
            .find(|story| story.family == StoryFamily::Turn)
            .expect("turn story publishes");
        assert_eq!(turn.headline, "implemented the release feed capture path");
        assert!(
            turn.deck
                .contains("Implemented the release feed capture path")
        );
        assert!(!turn.deck.contains("2 changed files"));
        assert!(!turn.deck.contains("tests passed"));
        assert!(!turn.deck.contains("cargo test --all"));
        assert_eq!(turn.chips[0], "agent_feed");
        assert_eq!(turn.chips[1], "codex");
        assert!(turn.lower_third.starts_with("agent_feed · codex"));
    }

    #[test]
    fn startup_context_does_not_flush_until_future_activity() {
        let mut compiler = StoryCompiler::default();
        let mut context = event(EventKind::AgentMessage, "codex posted an update");
        context.summary =
            Some("Browser feed discovery now follows signed story streams.".to_string());
        context.tags.push(STARTUP_CONTEXT_TAG.to_string());

        assert!(compiler.ingest(context).is_empty());
        assert!(compiler.flush().is_empty());

        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some("turn completed in 30s.".to_string());
        complete.score_hint = Some(82);
        let stories = compiler.ingest(complete);

        let turn = stories
            .iter()
            .find(|story| story.family == StoryFamily::Turn)
            .expect("future activity publishes with context");
        assert!(turn.headline.contains("browser feed discovery"));
    }

    #[test]
    fn turn_summary_rewrites_to_public_impact() {
        let mut compiler = StoryCompiler::default();
        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some("Implemented the p2p publisher identity display path.".to_string());
        complete.score_hint = Some(90);

        let stories = compiler.ingest(complete);

        assert_eq!(stories.len(), 1);
        assert_eq!(
            stories[0].headline,
            "remote feed headlines show verified publisher identity"
        );
        assert!(stories[0].deck.contains("github account"));
    }

    #[test]
    fn generic_implemented_and_published_status_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some("Implemented and published.".to_string());
        complete.score_hint = Some(84);

        assert!(compiler.ingest(complete).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn commit_push_narration_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary =
            Some("Implemented and pushed `fix: improve agent story capture quality`.".to_string());
        complete.score_hint = Some(90);

        assert!(compiler.ingest(complete).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn work_in_progress_narration_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some(
            "I'll check the installed Codex CLI help so the fix uses the actual option names."
                .to_string(),
        );
        complete.score_hint = Some(82);

        assert!(compiler.ingest(complete).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn generic_plan_update_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut plan = event(EventKind::PlanUpdate, "codex updated the plan");
        plan.summary = Some("plan updated.".to_string());
        plan.score_hint = Some(74);

        assert!(compiler.ingest(plan).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn active_verification_status_update_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some(
            "The focused Rust tests and CLI check are green. I'm now using the actual local transcript pipeline in dry-run mode.".to_string(),
        );
        complete.score_hint = Some(90);

        assert!(compiler.ingest(complete).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn model_only_turn_context_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut start = event(EventKind::TurnStart, "codex turn started");
        start.summary = Some("model gpt-5.5".to_string());
        start.score_hint = Some(45);
        assert!(compiler.ingest(start).is_empty());

        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some("turn completed in 12s.".to_string());
        complete.score_hint = Some(82);

        assert!(compiler.ingest(complete).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn generic_tool_failure_can_settle_with_meaningful_turn_summary() {
        let mut compiler = StoryCompiler::default();
        let mut failed = event(EventKind::ToolFail, "codex command failed");
        failed.summary = Some("exit 1. raw output omitted.".to_string());
        failed.command = Some("git status --short".to_string());
        failed.score_hint = Some(84);
        failed.severity = Severity::Warning;
        assert!(compiler.ingest(failed).is_empty());

        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary =
            Some("Improved browser feed error logging after the command failure.".to_string());
        complete.score_hint = Some(82);
        let stories = compiler.ingest(complete);

        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0].family, StoryFamily::Turn);
        assert_eq!(
            stories[0].headline,
            "feed surfaces capture and network failures more clearly"
        );
        assert!(!stories[0].deck.contains("1 tool failures"));
        assert!(!stories[0].headline.contains("shell command failed"));
    }

    #[test]
    fn duration_only_turn_completion_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some("turn completed in 304s.".to_string());
        complete.score_hint = Some(82);

        assert!(compiler.ingest(complete).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn interrupted_turn_has_specific_headline() {
        let mut compiler = StoryCompiler::default();
        let mut failed = event(EventKind::TurnFail, "codex turn failed");
        failed.summary = Some("interrupted by operator.".to_string());
        failed.score_hint = Some(92);
        failed.severity = Severity::Warning;
        let stories = compiler.ingest(failed);

        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0].headline, "codex was interrupted");
        assert!(!stories[0].headline.contains("hit agent_feed"));
    }

    #[test]
    fn severe_generic_tool_failure_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut failed = event(EventKind::ToolFail, "codex command failed");
        failed.summary = Some("exit 1. raw output omitted.".to_string());
        failed.score_hint = Some(92);
        failed.severity = Severity::Warning;

        let stories = compiler.ingest(failed);

        assert!(stories.is_empty());
    }

    #[test]
    fn generic_shell_failure_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut failed = event(EventKind::ToolFail, "codex command failed");
        failed.summary = Some("exit 1. raw output omitted.".to_string());
        failed.command = Some("sh -c cargo test".to_string());
        failed.score_hint = Some(84);
        failed.severity = Severity::Warning;

        assert!(compiler.ingest(failed).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn diagnostics_explain_rejected_story_gate() {
        let mut compiler = StoryCompiler::default();
        let mut failed = event(EventKind::ToolFail, "codex command failed");
        failed.summary = Some("exit 1. raw output omitted.".to_string());
        failed.command = Some("sh -c cargo test".to_string());
        failed.score_hint = Some(84);
        failed.severity = Severity::Warning;

        assert!(compiler.ingest(failed).is_empty());
        let diagnostics = compiler.diagnostics();

        assert_eq!(diagnostics.rejected_stories, 1);
        let decision = diagnostics
            .last_decision
            .expect("story gate decision is recorded");
        assert_eq!(decision.action, StoryDecisionAction::Rejected);
        assert_eq!(decision.family, StoryFamily::Incident);
        assert_eq!(
            decision.reason,
            "summary was too generic or mechanical to publish"
        );
        assert!(decision.score >= 80);
    }

    #[test]
    fn diagnostics_explain_waiting_story_gate() {
        let mut compiler = StoryCompiler::default();
        let mut command = event(EventKind::CommandExec, "codex command started");
        command.command = Some("cargo test --all".to_string());
        command.score_hint = Some(42);

        assert!(compiler.ingest(command).is_empty());
        let diagnostics = compiler.diagnostics();

        assert_eq!(diagnostics.open_windows, 2);
        let decision = diagnostics
            .last_decision
            .expect("waiting gate decision is recorded");
        assert_eq!(decision.action, StoryDecisionAction::Waiting);
        assert_eq!(decision.family, StoryFamily::Command);
        assert_eq!(
            decision.reason,
            "waiting for a completion, test, edit, or incident signal"
        );
    }

    #[test]
    fn command_burst_without_work_outcome_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        for index in 0..4 {
            let kind = if index % 2 == 0 {
                EventKind::CommandExec
            } else {
                EventKind::ToolComplete
            };
            let mut command = event(kind, "codex command completed");
            command.summary = Some(if kind == EventKind::CommandExec {
                "command lifecycle captured without command output.".to_string()
            } else {
                "exit 0. raw output omitted.".to_string()
            });
            command.command =
                Some("gh run view 24941390598 --repo example/project --json status".to_string());
            command.score_hint = Some(48);
            assert!(compiler.ingest(command).is_empty());
            if index == 1 {
                assert!(
                    compiler.flush().is_empty(),
                    "low-score turn windows should be retained until enough context arrives"
                );
            }
        }

        let stories = compiler.flush();

        assert!(
            stories.is_empty(),
            "ci polling and shell-only command bursts are not display stories"
        );
    }

    #[test]
    fn open_turn_project_verification_checkpoint_publishes() {
        let mut compiler = StoryCompiler::default();
        for index in 0..4 {
            let kind = if index % 2 == 0 {
                EventKind::CommandExec
            } else {
                EventKind::ToolComplete
            };
            let mut command = event(kind, "codex command completed");
            command.project = Some("burn_p2p".to_string());
            command.summary = Some(if kind == EventKind::CommandExec {
                "command lifecycle captured without command output.".to_string()
            } else {
                "exit 0. raw output omitted.".to_string()
            });
            command.command = Some("cargo test -p burn_p2p_browser".to_string());
            command.score_hint = Some(48);
            assert!(compiler.ingest(command).is_empty());
        }

        let stories = compiler.flush();

        assert_eq!(stories.len(), 1);
        assert_eq!(stories[0].project.as_deref(), Some("burn_p2p"));
        assert_eq!(stories[0].headline, "burn_p2p verification is active");
        assert!(
            stories[0]
                .deck
                .contains("final result has not been reported")
        );
        assert!(stories[0].score >= 72);
    }

    #[test]
    fn open_turn_project_checkpoint_does_not_hot_loop() {
        let mut compiler = StoryCompiler::default();
        for index in 0..4 {
            let mut command = event(EventKind::CommandExec, "codex command completed");
            command.project = Some("burn_dragon".to_string());
            command.summary =
                Some("command lifecycle captured without command output.".to_string());
            command.command = Some(format!(
                "gh run view {index} --repo aberration-technology/burn_dragon"
            ));
            command.score_hint = Some(48);
            assert!(compiler.ingest(command).is_empty());
        }

        assert_eq!(compiler.flush().len(), 1);
        assert!(compiler.flush().is_empty());

        let mut next_poll = event(EventKind::CommandExec, "codex command completed");
        next_poll.project = Some("burn_dragon".to_string());
        next_poll.summary = Some("command lifecycle captured without command output.".to_string());
        next_poll.command =
            Some("gh run view latest --repo aberration-technology/burn_dragon".to_string());
        next_poll.score_hint = Some(48);
        assert!(compiler.ingest(next_poll).is_empty());

        assert!(compiler.flush().is_empty());
        assert!(
            compiler
                .diagnostics()
                .recent_decisions
                .iter()
                .any(|decision| decision.action == StoryDecisionAction::Deduped
                    && decision.project.as_deref() == Some("burn_dragon")
                    && decision.family == StoryFamily::Turn)
        );
    }

    #[test]
    fn processor_json_summary_does_not_become_story_copy() {
        let mut compiler = StoryCompiler::default();
        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some(
            r#"{"headline":"codex changes two files, matches prior edit story","deck":"two changed files repeat the recent file-change summary.","publish":false,"memory_digest":"repeat"}"#
                .to_string(),
        );
        complete.score_hint = Some(82);

        assert!(compiler.ingest(complete).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn conversational_answer_summary_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some(
            "No, `agent-feed serve --p2p --all-workspaces` is not enough to publish.".to_string(),
        );
        complete.score_hint = Some(82);

        assert!(compiler.ingest(complete).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn absolute_path_summary_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some(
            "Done. Hero capture written to /home/mosure/repos/agent_feed/docs/image/hero.png."
                .to_string(),
        );
        complete.score_hint = Some(82);

        assert!(compiler.ingest(complete).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn standalone_test_status_without_context_does_not_publish() {
        let mut compiler = StoryCompiler::default();
        let mut pass = event(EventKind::TestPass, "tests passed");
        pass.summary = Some("test command passed.".to_string());
        pass.score_hint = Some(76);

        assert!(compiler.ingest(pass).is_empty());
        assert!(compiler.flush().is_empty());
    }

    #[test]
    fn compiled_story_ticker_is_display_safe() {
        let mut compiler = StoryCompiler::default();
        let mut complete = event(EventKind::TurnComplete, "codex turn completed");
        complete.summary = Some("Improved browser feed discovery for public users.".to_string());
        complete.score_hint = Some(82);
        let stories = compiler.ingest(complete);
        let ticker: TickerItem = stories[0].ticker_item();

        assert!(ticker.text.contains("codex"));
        assert!(!ticker.text.contains("raw"));
    }
}
