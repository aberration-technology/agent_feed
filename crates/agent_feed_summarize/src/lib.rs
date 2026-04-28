use agent_feed_core::{HeadlineImage, PrivacyClass, Severity};
use agent_feed_story::{CompiledStory, StoryFamily};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const PROCESSOR_TIMEOUT: Duration = Duration::from_secs(45);
const PROCESSOR_MAX_OUTPUT_BYTES: usize = 128 * 1024;
const HTTP_MAX_RESPONSE_BYTES: usize = 128 * 1024;
pub const DEFAULT_CODEX_SUMMARY_MODEL: &str = "gpt-5.3-codex-spark";

#[derive(Debug, thiserror::Error)]
pub enum SummaryError {
    #[error("summary guardrail rejected output: {0}")]
    GuardrailRejected(String),
    #[error("summary processor failed: {0}")]
    Processor(String),
    #[error("summary processor output was not utf-8")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("summary processor json failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryBudget {
    pub max_headline_chars: usize,
    pub max_deck_chars: usize,
    pub max_lower_third_chars: usize,
    pub max_chip_chars: usize,
    pub max_chips: usize,
    pub max_evidence_items: usize,
    pub max_capsule_chars: usize,
    pub max_feed_rollup_stories: usize,
}

impl Default for SummaryBudget {
    fn default() -> Self {
        Self {
            max_headline_chars: 96,
            max_deck_chars: 220,
            max_lower_third_chars: 120,
            max_chip_chars: 28,
            max_chips: 5,
            max_evidence_items: 8,
            max_capsule_chars: 720,
            max_feed_rollup_stories: 256,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailAction {
    Mask,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardrailPattern {
    pub name: String,
    pub pattern: String,
    pub replacement: String,
    pub action: GuardrailAction,
}

impl GuardrailPattern {
    #[must_use]
    pub fn mask(name: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pattern: pattern.into(),
            replacement: "[redacted]".to_string(),
            action: GuardrailAction::Mask,
        }
    }

    #[must_use]
    pub fn reject(name: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pattern: pattern.into(),
            replacement: "[redacted]".to_string(),
            action: GuardrailAction::Reject,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryGuardrails {
    pub name: String,
    pub version: u32,
    #[serde(default = "default_allow_project_tags")]
    pub allow_project_tags: bool,
    pub allow_project_names: bool,
    pub allow_local_paths: bool,
    pub allow_command_text: bool,
    pub allow_remote_identity: bool,
    pub patterns: Vec<GuardrailPattern>,
}

impl Default for SummaryGuardrails {
    fn default() -> Self {
        Self::strict_p2p()
    }
}

impl SummaryGuardrails {
    #[must_use]
    pub fn strict_p2p() -> Self {
        Self {
            name: "p2p-strict".to_string(),
            version: 1,
            allow_project_tags: true,
            allow_project_names: false,
            allow_local_paths: false,
            allow_command_text: false,
            allow_remote_identity: false,
            patterns: vec![
                GuardrailPattern::reject("openai-key", r"sk-[A-Za-z0-9_-]+"),
                GuardrailPattern::reject("github-token", r"gh[pousr]_[A-Za-z0-9_]+"),
                GuardrailPattern::reject("aws-key", r"AKIA[0-9A-Z]{16}"),
                GuardrailPattern::reject("private-key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
                GuardrailPattern::mask("email", r"(?i)[A-Z0-9._%+\-]+@[A-Z0-9.\-]+\.[A-Z]{2,}"),
                GuardrailPattern::mask("home-path", r"/home/[A-Za-z0-9_.\-]+"),
                GuardrailPattern::mask(
                    "credential-word",
                    r"(?i)\b(password|credential|secret|token|api[_-]?key|private-key)\b",
                ),
            ],
        }
    }

    pub fn clean_text(
        &self,
        input: &str,
    ) -> Result<(String, Vec<GuardrailViolation>), SummaryError> {
        let mut output = input.to_string();
        let mut violations = Vec::new();
        for pattern in &self.patterns {
            let regex = Regex::new(&pattern.pattern).map_err(|err| {
                SummaryError::Processor(format!(
                    "invalid guardrail pattern {}: {err}",
                    pattern.name
                ))
            })?;
            if !regex.is_match(&output) {
                continue;
            }
            violations.push(GuardrailViolation {
                name: pattern.name.clone(),
                action: pattern.action,
            });
            match pattern.action {
                GuardrailAction::Mask => {
                    output = regex
                        .replace_all(&output, pattern.replacement.as_str())
                        .to_string();
                }
                GuardrailAction::Reject => {
                    return Err(SummaryError::GuardrailRejected(pattern.name.clone()));
                }
            }
        }
        Ok((output, violations))
    }
}

fn default_allow_project_tags() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardrailViolation {
    pub name: String,
    pub action: GuardrailAction,
}

const PROMPT_LEAKAGE_VIOLATION: &str = "prompt-leakage";
pub const INTERNAL_SUMMARIZER_MARKER: &str = "agent-feed-internal-summarizer-v1";

const PROMPT_LEAKAGE_PATTERNS: &[&str] = &[
    r"(?i)\bagent-feed-internal-summarizer-v1\b[\s.,;:]*",
    r"(?i)\braw\s+prompts?,\s*command output,\s*diffs?,\s*paths?,\s*(?:and\s+)?repo names?\s+omitted\b[\s.,;:]*",
    r"(?i)\braw\s+(?:detail|details|diff|diffs|prompt|prompts|output|logs?|tool output|command output)\s+(?:is\s+|are\s+)?omitted\b[\s.,;:]*",
    r"(?i)\buse only the redacted story facts\b[\s.,;:]*",
    r"(?i)\bredacted story facts\b[\s.,;:]*",
    r"(?i)\breturn one json object\b[^.]*[.\s]*",
    r"(?i)\bdo not include\b[^.]*[.\s]*",
    r"(?i)\b(?:style guide|summary style|write with this style)\b[^.]*[.\s]*",
    r"(?i)\baustere technical broadcast\b[^.]*[.\s]*",
    r"(?i)\bno policy or omission text\b[\s.,;:]*",
];

pub const DEFAULT_SUMMARY_PROMPT_STYLE: &str = "austere technical broadcast; lowercase; compact news headline; lead with what shipped, what improved, what broke, or why it matters to users/operators/open-source consumers; keep codex/claude in chips, not headline; no ci polling, command-count, file-count, plan-state, or test-only status unless it explains meaningful impact; no milestone labels; no production/scaffold/gate/test-line or red/green test metaphors; no dashboard copy; no policy or omission text; no raw logs";
pub const DEFAULT_SUMMARY_PROMPT_MAX_CHARS: usize = 3000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedSummaryMode {
    PerStory,
    FeedRollup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SummaryProcessorConfig {
    Deterministic,
    CodexExec,
    CodexSessionMemory {
        store_path: String,
        key: String,
        command: String,
    },
    ClaudeCodeExec,
    Process {
        command: String,
        args: Vec<String>,
    },
    HttpEndpoint {
        url: String,
        auth_header_env: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageDecisionMode {
    BestJudgement,
    AlwaysAsk,
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageProcessorConfig {
    Disabled,
    CodexExec,
    ClaudeCodeExec,
    Process {
        command: String,
        args: Vec<String>,
    },
    HttpEndpoint {
        url: String,
        auth_header_env: Option<String>,
    },
}

impl ImageProcessorConfig {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::CodexExec => "codex-exec",
            Self::ClaudeCodeExec => "claude-code",
            Self::Process { .. } => "process",
            Self::HttpEndpoint { .. } => "http-endpoint",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    pub enabled: bool,
    pub processor: ImageProcessorConfig,
    pub decision: ImageDecisionMode,
    pub prompt_style: String,
    pub max_prompt_chars: usize,
    pub allow_remote_urls: bool,
    pub allowed_uri_prefixes: Vec<String>,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            processor: ImageProcessorConfig::Disabled,
            decision: ImageDecisionMode::BestJudgement,
            prompt_style: "austere technical broadcast; black field; off-white type; thin rules; no dashboard chrome; no raw logs; no secrets; no readable code; no brand impersonation".to_string(),
            max_prompt_chars: 1800,
            allow_remote_urls: false,
            allowed_uri_prefixes: vec![
                "/assets/headlines/".to_string(),
                "/media/headlines/".to_string(),
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryPromptConfig {
    pub style: String,
    pub max_prompt_chars: usize,
}

impl Default for SummaryPromptConfig {
    fn default() -> Self {
        Self {
            style: DEFAULT_SUMMARY_PROMPT_STYLE.to_string(),
            max_prompt_chars: DEFAULT_SUMMARY_PROMPT_MAX_CHARS,
        }
    }
}

impl SummaryProcessorConfig {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::CodexExec => "codex-exec",
            Self::CodexSessionMemory { .. } => "codex-memory",
            Self::ClaudeCodeExec => "claude-code",
            Self::Process { .. } => "process",
            Self::HttpEndpoint { .. } => "http-endpoint",
        }
    }

    #[must_use]
    pub fn codex_command() -> Self {
        Self::Process {
            command: "codex".to_string(),
            args: vec![
                "exec".to_string(),
                "--json".to_string(),
                "--model".to_string(),
                DEFAULT_CODEX_SUMMARY_MODEL.to_string(),
            ],
        }
    }

    #[must_use]
    pub fn claude_command() -> Self {
        Self::Process {
            command: "claude".to_string(),
            args: vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
            ],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryConfig {
    pub mode: FeedSummaryMode,
    pub processor: SummaryProcessorConfig,
    #[serde(default)]
    pub prompt: SummaryPromptConfig,
    pub image: ImageConfig,
    pub publish: PublishDecisionConfig,
    pub budget: SummaryBudget,
    pub guardrails: SummaryGuardrails,
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self::p2p_default()
    }
}

impl SummaryConfig {
    #[must_use]
    pub fn p2p_default() -> Self {
        Self {
            mode: FeedSummaryMode::FeedRollup,
            processor: SummaryProcessorConfig::Deterministic,
            prompt: SummaryPromptConfig::default(),
            image: ImageConfig::default(),
            publish: PublishDecisionConfig::default(),
            budget: SummaryBudget::default(),
            guardrails: SummaryGuardrails::strict_p2p(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishDecisionConfig {
    pub enabled: bool,
    pub allow_processor_skip: bool,
    pub recent_window: usize,
    pub max_headline_similarity: u8,
    pub max_deck_similarity_when_headline_matches: u8,
    pub severe_score_bypass: u8,
}

impl Default for PublishDecisionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_processor_skip: true,
            recent_window: 24,
            max_headline_similarity: 88,
            max_deck_similarity_when_headline_matches: 82,
            severe_score_bypass: 90,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishAction {
    Publish,
    SkipDuplicate,
    SkipProcessor,
}

impl PublishAction {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Publish => "publish",
            Self::SkipDuplicate => "skip_duplicate",
            Self::SkipProcessor => "skip_processor",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentSummary {
    pub headline: String,
    pub deck: String,
    pub story_family: StoryFamily,
    pub score: u8,
}

impl From<&FeedSummary> for RecentSummary {
    fn from(summary: &FeedSummary) -> Self {
        Self {
            headline: summary.headline.clone(),
            deck: summary.deck.clone(),
            story_family: summary.story_family,
            score: summary.score,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SummaryRequest {
    pub feed_id: String,
    pub mode: FeedSummaryMode,
    pub stories: Vec<CompiledStory>,
    #[serde(default)]
    pub recent_summaries: Vec<RecentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prompt_chars: Option<usize>,
    pub branch: Option<String>,
    pub session_hint: Option<String>,
}

impl SummaryRequest {
    #[must_use]
    pub fn new(
        feed_id: impl Into<String>,
        mode: FeedSummaryMode,
        stories: Vec<CompiledStory>,
    ) -> Self {
        Self {
            feed_id: feed_id.into(),
            mode,
            stories,
            recent_summaries: Vec::new(),
            prompt_style: None,
            max_prompt_chars: None,
            branch: None,
            session_hint: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeedSummary {
    pub story_window: String,
    pub source_agent_kinds: Vec<String>,
    pub headline: String,
    pub deck: String,
    pub lower_third: String,
    pub chips: Vec<String>,
    pub image: Option<HeadlineImage>,
    pub story_family: StoryFamily,
    pub severity: Severity,
    pub score: u8,
    pub privacy_class: PrivacyClass,
    pub evidence_event_ids: Vec<String>,
    pub metadata: SummaryMetadata,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SummaryMetadata {
    pub processor: String,
    pub policy: String,
    pub image_processor: String,
    pub publish_action: PublishAction,
    pub publish_reason: String,
    pub headline_fingerprint: String,
    pub max_headline_similarity: u8,
    pub max_deck_similarity: u8,
    pub guardrail_version: u32,
    pub input_stories: usize,
    pub output_chars: usize,
    pub external_cost_allowed: bool,
    pub image_enabled: bool,
    pub violations: Vec<GuardrailViolation>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProcessorSummary {
    pub headline: String,
    pub deck: String,
    #[serde(default)]
    pub lower_third: Option<String>,
    #[serde(default)]
    pub chips: Vec<String>,
    #[serde(default)]
    pub publish: Option<bool>,
    #[serde(default)]
    pub publish_reason: Option<String>,
    #[serde(default)]
    pub memory_digest: Option<String>,
    #[serde(default)]
    pub semantic_fingerprint: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProcessorImage {
    pub uri: String,
    pub alt: String,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProcessorImageResponse {
    #[serde(default)]
    pub image: Option<ProcessorImage>,
    #[serde(default)]
    pub reason: Option<String>,
}

pub trait SummaryProcessor {
    fn name(&self) -> &str;

    fn summarize(&self, request: &SummaryRequest) -> Result<ProcessorSummary, SummaryError>;
}

pub trait ImageProcessor {
    fn name(&self) -> &str;

    fn summarize_image(
        &self,
        request: &ImageRequest,
    ) -> Result<Option<HeadlineImage>, SummaryError>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageRequest {
    pub feed_id: String,
    pub headline: String,
    pub deck: String,
    pub lower_third: String,
    pub chips: Vec<String>,
    pub story_family: StoryFamily,
    pub severity: Severity,
    pub score: u8,
    pub policy: ImageConfig,
}

pub struct ExternalProcessProcessor {
    command: String,
    args: Vec<String>,
}

impl ExternalProcessProcessor {
    #[must_use]
    pub fn new(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
        }
    }
}

impl SummaryProcessor for ExternalProcessProcessor {
    fn name(&self) -> &str {
        "process"
    }

    fn summarize(&self, request: &SummaryRequest) -> Result<ProcessorSummary, SummaryError> {
        let prompt = processor_prompt(request);
        let stdout = run_process(&self.command, &self.args, &prompt)?;
        parse_processor_output(&stdout)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SummaryMemoryStore {
    records: BTreeMap<String, SummaryMemoryRecord>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SummaryMemoryRecord {
    codex_session_id: Option<String>,
    memory_digest: Option<String>,
    semantic_fingerprint: Option<String>,
    uses: u64,
}

pub struct CodexSessionMemoryProcessor {
    store_path: PathBuf,
    key: String,
    command: String,
}

impl CodexSessionMemoryProcessor {
    #[must_use]
    pub fn new(
        store_path: impl Into<PathBuf>,
        key: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self {
            store_path: store_path.into(),
            key: key.into(),
            command: command.into(),
        }
    }

    fn load_store(&self) -> Result<SummaryMemoryStore, SummaryError> {
        if !self.store_path.exists() {
            return Ok(SummaryMemoryStore::default());
        }
        let input = fs::read_to_string(&self.store_path).map_err(|err| {
            SummaryError::Processor(format!(
                "summary memory read failed for {}: {err}",
                self.store_path.display()
            ))
        })?;
        serde_json::from_str(&input).map_err(SummaryError::from)
    }

    fn save_store(&self, store: &SummaryMemoryStore) -> Result<(), SummaryError> {
        if let Some(parent) = self.store_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|err| {
                SummaryError::Processor(format!(
                    "summary memory directory create failed for {}: {err}",
                    parent.display()
                ))
            })?;
        }
        let body = serde_json::to_string_pretty(store)?;
        fs::write(&self.store_path, body).map_err(|err| {
            SummaryError::Processor(format!(
                "summary memory write failed for {}: {err}",
                self.store_path.display()
            ))
        })
    }

    fn prompt_with_memory(
        &self,
        request: &SummaryRequest,
        record: Option<&SummaryMemoryRecord>,
    ) -> String {
        let digest = record
            .and_then(|record| record.memory_digest.as_deref())
            .filter(|digest| !digest.trim().is_empty())
            .unwrap_or("none");
        let fingerprint = record
            .and_then(|record| record.semantic_fingerprint.as_deref())
            .filter(|fingerprint| !fingerprint.trim().is_empty())
            .unwrap_or("none");
        format!(
            "You are the private local headline memory for one agent feed. Maintain continuity across calls, but publish only when the new redacted delta changes the public story. Do not run tools. Return JSON only. Include memory_digest and semantic_fingerprint.\nsummary_memory_key={}\nprior_memory_digest={}\nprior_semantic_fingerprint={}\n\n{}",
            self.key,
            clamp_chars(digest, 900),
            clamp_chars(fingerprint, 180),
            processor_prompt(request)
        )
    }

    fn work_dir(&self) -> PathBuf {
        self.store_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join("codex-memory-work")
    }
}

impl SummaryProcessor for CodexSessionMemoryProcessor {
    fn name(&self) -> &str {
        "codex-memory"
    }

    fn summarize(&self, request: &SummaryRequest) -> Result<ProcessorSummary, SummaryError> {
        let mut store = self.load_store()?;
        let existing = store.records.get(&self.key).cloned();
        let prompt = self.prompt_with_memory(request, existing.as_ref());
        let args = if let Some(session_id) = existing
            .as_ref()
            .and_then(|record| record.codex_session_id.as_deref())
            .filter(|session_id| !session_id.trim().is_empty())
        {
            vec![
                "exec".to_string(),
                "resume".to_string(),
                "--json".to_string(),
                "--model".to_string(),
                DEFAULT_CODEX_SUMMARY_MODEL.to_string(),
                "--skip-git-repo-check".to_string(),
                session_id.to_string(),
                "-".to_string(),
            ]
        } else {
            vec![
                "exec".to_string(),
                "--json".to_string(),
                "--model".to_string(),
                DEFAULT_CODEX_SUMMARY_MODEL.to_string(),
                "--sandbox".to_string(),
                "read-only".to_string(),
                "--skip-git-repo-check".to_string(),
                "-".to_string(),
            ]
        };
        let work_dir = self.work_dir();
        fs::create_dir_all(&work_dir).map_err(|err| {
            SummaryError::Processor(format!(
                "codex memory work directory create failed for {}: {err}",
                work_dir.display()
            ))
        })?;
        let stdout = run_process_with_options(
            &self.command,
            &args,
            &prompt,
            ProcessOptions {
                current_dir: Some(work_dir),
            },
        )?;
        let ParsedProcessorOutput {
            summary,
            codex_session_id,
        } = parse_processor_output_with_meta(&stdout)?;
        let mut record = existing.unwrap_or_default();
        if codex_session_id.is_some() {
            record.codex_session_id = codex_session_id;
        }
        if let Some(digest) = summary.memory_digest.as_ref() {
            record.memory_digest = Some(clamp_chars(digest, 1400));
        } else if record.memory_digest.is_none() {
            record.memory_digest = Some(clamp_chars(
                &format!("{} {}", summary.headline, summary.deck),
                1400,
            ));
        }
        if let Some(fingerprint) = summary.semantic_fingerprint.as_ref() {
            record.semantic_fingerprint = Some(clamp_chars(fingerprint, 180));
        } else {
            record.semantic_fingerprint = Some(headline_fingerprint(&summary.headline));
        }
        record.uses = record.uses.saturating_add(1);
        store.records.insert(self.key.clone(), record);
        self.save_store(&store)?;
        Ok(summary)
    }
}

pub struct ExternalImageProcessor {
    command: String,
    args: Vec<String>,
}

impl ExternalImageProcessor {
    #[must_use]
    pub fn new(command: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            command: command.into(),
            args,
        }
    }
}

impl ImageProcessor for ExternalImageProcessor {
    fn name(&self) -> &str {
        "process"
    }

    fn summarize_image(
        &self,
        request: &ImageRequest,
    ) -> Result<Option<HeadlineImage>, SummaryError> {
        let prompt = image_processor_prompt(request);
        let stdout = run_process(&self.command, &self.args, &prompt)?;
        parse_image_processor_output(&stdout, &request.policy, self.name())
    }
}

#[derive(Clone, Debug, Default)]
struct ProcessOptions {
    current_dir: Option<PathBuf>,
}

fn run_process(command: &str, args: &[String], stdin_text: &str) -> Result<String, SummaryError> {
    run_process_with_options(command, args, stdin_text, ProcessOptions::default())
}

fn run_process_with_options(
    command: &str,
    args: &[String],
    stdin_text: &str,
    options: ProcessOptions,
) -> Result<String, SummaryError> {
    let mut command_builder = Command::new(command);
    command_builder
        .args(args)
        .env("AGENT_FEED_INTERNAL_PROCESSOR", "summary")
        .env("AGENT_FEED_CAPTURE_DISABLED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(current_dir) = options.current_dir {
        command_builder.current_dir(current_dir);
    }
    let mut child = command_builder
        .spawn()
        .map_err(|err| SummaryError::Processor(format!("spawn {command} failed: {err}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| SummaryError::Processor("processor stdin unavailable".to_string()))?;
    stdin
        .write_all(stdin_text.as_bytes())
        .map_err(|err| SummaryError::Processor(format!("write stdin failed: {err}")))?;
    drop(stdin);

    let deadline = Instant::now() + PROCESSOR_TIMEOUT;
    loop {
        if child
            .try_wait()
            .map_err(|err| SummaryError::Processor(format!("wait failed: {err}")))?
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SummaryError::Processor(format!(
                "{command} timed out after {} seconds",
                PROCESSOR_TIMEOUT.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(25));
    }

    let output = child
        .wait_with_output()
        .map_err(|err| SummaryError::Processor(format!("wait failed: {err}")))?;
    if output.stdout.len() > PROCESSOR_MAX_OUTPUT_BYTES
        || output.stderr.len() > PROCESSOR_MAX_OUTPUT_BYTES
    {
        return Err(SummaryError::Processor(format!(
            "{command} output exceeded {} bytes",
            PROCESSOR_MAX_OUTPUT_BYTES
        )));
    }
    if !output.status.success() {
        return Err(SummaryError::Processor(format!(
            "{command} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    String::from_utf8(output.stdout).map_err(SummaryError::from)
}

pub struct HttpEndpointProcessor {
    url: String,
    auth_header: Option<String>,
}

impl HttpEndpointProcessor {
    pub fn from_env(
        url: impl Into<String>,
        auth_header_env: Option<&str>,
    ) -> Result<Self, SummaryError> {
        let auth_header = auth_header_env
            .map(|name| {
                env::var(name).map_err(|err| {
                    SummaryError::Processor(format!(
                        "summary endpoint auth env {name} unavailable: {err}"
                    ))
                })
            })
            .transpose()?;
        Ok(Self {
            url: url.into(),
            auth_header,
        })
    }
}

impl SummaryProcessor for HttpEndpointProcessor {
    fn name(&self) -> &str {
        "http-endpoint"
    }

    fn summarize(&self, request: &SummaryRequest) -> Result<ProcessorSummary, SummaryError> {
        let body = serde_json::to_vec(request)?;
        let text = post_http_json(&self.url, self.auth_header.as_deref(), &body)?;
        parse_processor_output(&text)
    }
}

pub struct HttpImageEndpointProcessor {
    url: String,
    auth_header: Option<String>,
}

impl HttpImageEndpointProcessor {
    pub fn from_env(
        url: impl Into<String>,
        auth_header_env: Option<&str>,
    ) -> Result<Self, SummaryError> {
        let auth_header = auth_header_env
            .map(|name| {
                env::var(name).map_err(|err| {
                    SummaryError::Processor(format!(
                        "image endpoint auth env {name} unavailable: {err}"
                    ))
                })
            })
            .transpose()?;
        Ok(Self {
            url: url.into(),
            auth_header,
        })
    }
}

impl ImageProcessor for HttpImageEndpointProcessor {
    fn name(&self) -> &str {
        "http-endpoint"
    }

    fn summarize_image(
        &self,
        request: &ImageRequest,
    ) -> Result<Option<HeadlineImage>, SummaryError> {
        let body = serde_json::to_vec(request)?;
        let text = post_http_json(&self.url, self.auth_header.as_deref(), &body)?;
        parse_image_processor_output(&text, &request.policy, self.name())
    }
}

struct HttpEndpoint {
    host: String,
    port: u16,
    path: String,
}

impl HttpEndpoint {
    fn parse(url: &str) -> Result<Self, SummaryError> {
        let rest = url.strip_prefix("http://").ok_or_else(|| {
            SummaryError::Processor(
                "http endpoint processor supports http:// endpoints; install a custom processor for https"
                    .to_string(),
            )
        })?;
        let (authority, path) = rest
            .split_once('/')
            .map(|(authority, path)| (authority, format!("/{path}")))
            .unwrap_or((rest, "/".to_string()));
        let (host, port) = authority
            .rsplit_once(':')
            .map(|(host, port)| {
                port.parse::<u16>()
                    .map(|port| (host.to_string(), port))
                    .map_err(|err| SummaryError::Processor(format!("invalid endpoint port: {err}")))
            })
            .transpose()?
            .unwrap_or_else(|| (authority.to_string(), 80));
        if host.is_empty() {
            return Err(SummaryError::Processor(
                "http endpoint host is empty".to_string(),
            ));
        }
        Ok(Self { host, port, path })
    }
}

fn post_http_json(
    url: &str,
    auth_header: Option<&str>,
    body: &[u8],
) -> Result<String, SummaryError> {
    let endpoint = HttpEndpoint::parse(url)?;
    let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .map_err(|err| SummaryError::Processor(format!("http endpoint connect failed: {err}")))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|err| SummaryError::Processor(format!("http read timeout failed: {err}")))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(20)))
        .map_err(|err| SummaryError::Processor(format!("http write timeout failed: {err}")))?;

    write!(
        stream,
        "POST {} HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        endpoint.path,
        endpoint.host,
        body.len()
    )
    .map_err(|err| SummaryError::Processor(format!("http request write failed: {err}")))?;
    if let Some(auth_header) = auth_header {
        write!(stream, "authorization: {auth_header}\r\n").map_err(|err| {
            SummaryError::Processor(format!("http auth header write failed: {err}"))
        })?;
    }
    stream
        .write_all(b"\r\n")
        .and_then(|()| stream.write_all(body))
        .map_err(|err| SummaryError::Processor(format!("http body write failed: {err}")))?;

    let mut response = String::new();
    stream
        .take((HTTP_MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_string(&mut response)
        .map_err(|err| SummaryError::Processor(format!("http response read failed: {err}")))?;
    if response.len() > HTTP_MAX_RESPONSE_BYTES {
        return Err(SummaryError::Processor(format!(
            "http endpoint response exceeded {HTTP_MAX_RESPONSE_BYTES} bytes"
        )));
    }
    let (status, text) = parse_http_response(&response)?;
    if !(200..300).contains(&status) {
        return Err(SummaryError::Processor(format!(
            "http endpoint returned {status}: {}",
            clamp_chars(text.trim(), 240)
        )));
    }
    Ok(text.to_string())
}

fn parse_http_response(response: &str) -> Result<(u16, &str), SummaryError> {
    let (headers, body) = response.split_once("\r\n\r\n").ok_or_else(|| {
        SummaryError::Processor("http endpoint response missing headers".to_string())
    })?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| SummaryError::Processor("http endpoint status missing".to_string()))?
        .parse::<u16>()
        .map_err(|err| SummaryError::Processor(format!("http endpoint status invalid: {err}")))?;
    Ok((status, body))
}

pub fn summarize_feed(
    feed_id: &str,
    stories: &[CompiledStory],
    config: &SummaryConfig,
) -> Result<Vec<FeedSummary>, SummaryError> {
    summarize_feed_inner(feed_id, stories, config, None, None, &[])
}

pub fn summarize_feed_with_recent(
    feed_id: &str,
    stories: &[CompiledStory],
    config: &SummaryConfig,
    recent_summaries: &[RecentSummary],
) -> Result<Vec<FeedSummary>, SummaryError> {
    summarize_feed_inner(feed_id, stories, config, None, None, recent_summaries)
}

pub fn summarize_feed_with_processor(
    feed_id: &str,
    stories: &[CompiledStory],
    config: &SummaryConfig,
    processor: &dyn SummaryProcessor,
) -> Result<Vec<FeedSummary>, SummaryError> {
    summarize_feed_inner(feed_id, stories, config, Some(processor), None, &[])
}

pub fn summarize_feed_with_processors(
    feed_id: &str,
    stories: &[CompiledStory],
    config: &SummaryConfig,
    processor: Option<&dyn SummaryProcessor>,
    image_processor: Option<&dyn ImageProcessor>,
) -> Result<Vec<FeedSummary>, SummaryError> {
    summarize_feed_inner(feed_id, stories, config, processor, image_processor, &[])
}

fn summarize_feed_inner(
    feed_id: &str,
    stories: &[CompiledStory],
    config: &SummaryConfig,
    processor: Option<&dyn SummaryProcessor>,
    image_processor: Option<&dyn ImageProcessor>,
    recent_summaries: &[RecentSummary],
) -> Result<Vec<FeedSummary>, SummaryError> {
    if stories.is_empty() {
        tracing::debug!(%feed_id, "summary skipped empty story batch");
        return Ok(Vec::new());
    }

    tracing::info!(
        %feed_id,
        mode = ?config.mode,
        processor = summary_processor_name(config, processor),
        stories = stories.len(),
        recent_summaries = recent_summaries.len(),
        max_rollup_stories = config.budget.max_feed_rollup_stories,
        publish_policy_enabled = config.publish.enabled,
        "summary evaluation started"
    );

    let batches: Vec<Vec<CompiledStory>> = match config.mode {
        FeedSummaryMode::PerStory => stories.iter().cloned().map(|story| vec![story]).collect(),
        FeedSummaryMode::FeedRollup => stories
            .chunks(config.budget.max_feed_rollup_stories.max(1))
            .map(<[CompiledStory]>::to_vec)
            .collect(),
    };

    let mut summaries = Vec::new();
    let mut recent = recent_summaries
        .iter()
        .take(config.publish.recent_window.max(1))
        .cloned()
        .collect::<VecDeque<_>>();
    for batch in batches {
        let mut request = SummaryRequest::new(feed_id, config.mode, batch.clone());
        request.recent_summaries = recent.iter().cloned().collect();
        match summarize_request_inner(&request, config, processor, image_processor) {
            Ok(summary) => {
                push_publishable_summary(
                    summary,
                    &request,
                    &config.publish,
                    &mut recent,
                    &mut summaries,
                );
            }
            Err(SummaryError::GuardrailRejected(_)) if batch.len() > 1 => {
                tracing::info!(
                    %feed_id,
                    batch_stories = batch.len(),
                    "summary rollup rejected by guardrails; retrying per-story"
                );
                for story in batch {
                    let mut single_request =
                        SummaryRequest::new(feed_id, FeedSummaryMode::PerStory, vec![story]);
                    single_request.recent_summaries = recent.iter().cloned().collect();
                    match summarize_request_inner(
                        &single_request,
                        config,
                        processor,
                        image_processor,
                    ) {
                        Ok(summary) => push_publishable_summary(
                            summary,
                            &single_request,
                            &config.publish,
                            &mut recent,
                            &mut summaries,
                        ),
                        Err(SummaryError::GuardrailRejected(_)) => {
                            tracing::info!(
                                %feed_id,
                                "summary story rejected by guardrails"
                            );
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
            Err(SummaryError::GuardrailRejected(_)) => {
                tracing::info!(
                    %feed_id,
                    batch_stories = batch.len(),
                    "summary batch rejected by guardrails"
                );
            }
            Err(err) => return Err(err),
        }
    }
    tracing::info!(
        %feed_id,
        summaries = summaries.len(),
        "summary evaluation completed"
    );
    Ok(summaries)
}

fn push_publishable_summary(
    mut summary: FeedSummary,
    request: &SummaryRequest,
    policy: &PublishDecisionConfig,
    recent: &mut VecDeque<RecentSummary>,
    summaries: &mut Vec<FeedSummary>,
) {
    if !apply_publish_decision(&mut summary, request, policy) {
        tracing::info!(
            feed_id = %request.feed_id,
            action = summary.metadata.publish_action.as_str(),
            reason = %summary.metadata.publish_reason,
            processor = %summary.metadata.processor,
            story_family = ?summary.story_family,
            score = summary.score,
            input_stories = summary.metadata.input_stories,
            output_chars = summary.metadata.output_chars,
            headline_similarity = summary.metadata.max_headline_similarity,
            deck_similarity = summary.metadata.max_deck_similarity,
            violations = summary.metadata.violations.len(),
            headline_fingerprint = %summary.metadata.headline_fingerprint,
            "summary publish decision skipped"
        );
        return;
    }
    tracing::info!(
        feed_id = %request.feed_id,
        action = summary.metadata.publish_action.as_str(),
        reason = %summary.metadata.publish_reason,
        processor = %summary.metadata.processor,
        story_family = ?summary.story_family,
        score = summary.score,
        input_stories = summary.metadata.input_stories,
        output_chars = summary.metadata.output_chars,
        headline_similarity = summary.metadata.max_headline_similarity,
        deck_similarity = summary.metadata.max_deck_similarity,
        violations = summary.metadata.violations.len(),
        image_enabled = summary.metadata.image_enabled,
        image_attached = summary.image.is_some(),
        headline_fingerprint = %summary.metadata.headline_fingerprint,
        "summary publish decision accepted"
    );
    recent.push_front(RecentSummary::from(&summary));
    while recent.len() > policy.recent_window.max(1) {
        recent.pop_back();
    }
    summaries.push(summary);
}

fn summary_processor_name(
    config: &SummaryConfig,
    processor: Option<&dyn SummaryProcessor>,
) -> &'static str {
    if processor.is_some() {
        return "custom";
    }
    match &config.processor {
        SummaryProcessorConfig::Deterministic => "deterministic",
        SummaryProcessorConfig::CodexExec => "codex-exec",
        SummaryProcessorConfig::CodexSessionMemory { .. } => "codex-memory",
        SummaryProcessorConfig::ClaudeCodeExec => "claude-code",
        SummaryProcessorConfig::Process { .. } => "process",
        SummaryProcessorConfig::HttpEndpoint { .. } => "http-endpoint",
    }
}

pub fn summarize_request(
    request: &SummaryRequest,
    config: &SummaryConfig,
) -> Result<FeedSummary, SummaryError> {
    summarize_request_inner(request, config, None, None)
}

pub fn summarize_request_with_processor(
    request: &SummaryRequest,
    config: &SummaryConfig,
    processor: &dyn SummaryProcessor,
) -> Result<FeedSummary, SummaryError> {
    summarize_request_inner(request, config, Some(processor), None)
}

pub fn summarize_request_with_processors(
    request: &SummaryRequest,
    config: &SummaryConfig,
    processor: Option<&dyn SummaryProcessor>,
    image_processor: Option<&dyn ImageProcessor>,
) -> Result<FeedSummary, SummaryError> {
    summarize_request_inner(request, config, processor, image_processor)
}

fn summarize_request_inner(
    request: &SummaryRequest,
    config: &SummaryConfig,
    processor: Option<&dyn SummaryProcessor>,
    image_processor: Option<&dyn ImageProcessor>,
) -> Result<FeedSummary, SummaryError> {
    tracing::debug!(
        feed_id = %request.feed_id,
        mode = ?request.mode,
        processor = summary_processor_name(config, processor),
        stories = request.stories.len(),
        recent_summaries = request.recent_summaries.len(),
        prompt_chars = config.prompt.max_prompt_chars,
        external_cost_allowed = !matches!(&config.processor, SummaryProcessorConfig::Deterministic),
        "summary processor preparing candidate"
    );
    let processor_output = if let Some(processor) = processor {
        let processor_request = request_for_external_processor(request, config)?;
        processor.summarize(&processor_request)?
    } else {
        match &config.processor {
            SummaryProcessorConfig::Deterministic => deterministic_output(request),
            SummaryProcessorConfig::CodexExec => {
                let processor_request = request_for_external_processor(request, config)?;
                ExternalProcessProcessor::new(
                    "codex",
                    vec![
                        "exec".to_string(),
                        "--json".to_string(),
                        "--model".to_string(),
                        DEFAULT_CODEX_SUMMARY_MODEL.to_string(),
                    ],
                )
                .summarize(&processor_request)?
            }
            SummaryProcessorConfig::CodexSessionMemory {
                store_path,
                key,
                command,
            } => {
                let processor_request = request_for_external_processor(request, config)?;
                match CodexSessionMemoryProcessor::new(store_path, key, command)
                    .summarize(&processor_request)
                {
                    Ok(summary) => summary,
                    Err(err) => {
                        let mut summary = deterministic_output(request);
                        summary.publish_reason = Some(format!(
                            "codex-memory unavailable; deterministic fallback used: {err}"
                        ));
                        summary
                    }
                }
            }
            SummaryProcessorConfig::ClaudeCodeExec => ExternalProcessProcessor::new(
                "claude",
                vec![
                    "--print".to_string(),
                    "--output-format".to_string(),
                    "json".to_string(),
                ],
            )
            .summarize(&request_for_external_processor(request, config)?)?,
            SummaryProcessorConfig::Process { command, args } => {
                let processor_request = request_for_external_processor(request, config)?;
                ExternalProcessProcessor::new(command, args.clone())
                    .summarize(&processor_request)?
            }
            SummaryProcessorConfig::HttpEndpoint {
                url,
                auth_header_env,
            } => {
                let processor_request = request_for_external_processor(request, config)?;
                HttpEndpointProcessor::from_env(url, auth_header_env.as_deref())?
                    .summarize(&processor_request)?
            }
        }
    };

    let mut summary = build_summary(request, processor_output, config)?;
    attach_optional_image(&mut summary, request, config, image_processor)?;
    tracing::debug!(
        feed_id = %request.feed_id,
        processor = %summary.metadata.processor,
        story_family = ?summary.story_family,
        score = summary.score,
        input_stories = summary.metadata.input_stories,
        output_chars = summary.metadata.output_chars,
        violations = summary.metadata.violations.len(),
        image_enabled = summary.metadata.image_enabled,
        image_attached = summary.image.is_some(),
        "summary processor produced candidate"
    );
    Ok(summary)
}

fn request_for_external_processor(
    request: &SummaryRequest,
    config: &SummaryConfig,
) -> Result<SummaryRequest, SummaryError> {
    let mut redacted = request.clone();
    redacted.prompt_style = Some(clamp_chars(&config.prompt.style, 512));
    redacted.max_prompt_chars = Some(config.prompt.max_prompt_chars.max(512));
    redacted.branch = request.branch.as_ref().map(|_| "[redacted]".to_string());
    redacted.session_hint = request
        .session_hint
        .as_ref()
        .map(|_| "[redacted]".to_string());
    for story in &mut redacted.stories {
        story.key.project_hash = None;
        if !config.guardrails.allow_project_names {
            story.project = if config.guardrails.allow_project_tags {
                story.project.as_deref().and_then(public_project_tag)
            } else {
                None
            };
        }
        let project_tags =
            project_tags_for_stories(std::slice::from_ref(story), &config.guardrails);
        story.headline = clean_and_clamp_with_project_tags(
            &story.headline,
            config.budget.max_headline_chars,
            &config.guardrails,
            &project_tags,
        )?
        .0;
        story.deck = clean_and_clamp_with_project_tags(
            &story.deck,
            config.budget.max_deck_chars,
            &config.guardrails,
            &project_tags,
        )?
        .0;
        story.lower_third = clean_and_clamp_with_project_tags(
            &story.lower_third,
            config.budget.max_lower_third_chars,
            &config.guardrails,
            &project_tags,
        )?
        .0;
        story.chips = story
            .chips
            .iter()
            .take(config.budget.max_chips)
            .map(|chip| {
                clean_chip(
                    chip,
                    config.budget.max_chip_chars,
                    &config.guardrails,
                    &project_tags,
                )
                .map(|(chip, _)| chip)
            })
            .collect::<Result<Vec<_>, _>>()?;
    }
    redacted.recent_summaries = redacted
        .recent_summaries
        .iter()
        .take(config.publish.recent_window.max(1))
        .map(|summary| {
            Ok(RecentSummary {
                headline: clean_and_clamp(
                    &summary.headline,
                    config.budget.max_headline_chars,
                    &config.guardrails,
                )?
                .0,
                deck: clean_and_clamp(
                    &summary.deck,
                    config.budget.max_deck_chars,
                    &config.guardrails,
                )?
                .0,
                story_family: summary.story_family,
                score: summary.score,
            })
        })
        .collect::<Result<Vec<_>, SummaryError>>()?;
    Ok(redacted)
}

fn attach_optional_image(
    summary: &mut FeedSummary,
    request: &SummaryRequest,
    config: &SummaryConfig,
    image_processor: Option<&dyn ImageProcessor>,
) -> Result<(), SummaryError> {
    if !config.image.enabled || config.image.decision == ImageDecisionMode::Never {
        return Ok(());
    }
    if !image_warranted(summary, &config.image) {
        return Ok(());
    }
    let image_request = ImageRequest {
        feed_id: request.feed_id.clone(),
        headline: summary.headline.clone(),
        deck: summary.deck.clone(),
        lower_third: summary.lower_third.clone(),
        chips: summary.chips.clone(),
        story_family: summary.story_family,
        severity: summary.severity,
        score: summary.score,
        policy: config.image.clone(),
    };
    let image = if let Some(processor) = image_processor {
        processor.summarize_image(&image_request)?
    } else if let Some(processor) = configured_image_processor(&config.image)? {
        processor.summarize_image(&image_request)?
    } else {
        None
    };
    if let Some(image) = image.as_ref() {
        validate_headline_image(image, &config.image)?;
    }
    summary.image = image;
    Ok(())
}

fn configured_image_processor(
    config: &ImageConfig,
) -> Result<Option<Box<dyn ImageProcessor>>, SummaryError> {
    let processor: Box<dyn ImageProcessor> = match &config.processor {
        ImageProcessorConfig::Disabled => return Ok(None),
        ImageProcessorConfig::CodexExec => Box::new(ExternalImageProcessor::new(
            "codex",
            vec!["exec".to_string(), "--json".to_string()],
        )),
        ImageProcessorConfig::ClaudeCodeExec => Box::new(ExternalImageProcessor::new(
            "claude",
            vec![
                "--print".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
            ],
        )),
        ImageProcessorConfig::Process { command, args } => {
            Box::new(ExternalImageProcessor::new(command, args.clone()))
        }
        ImageProcessorConfig::HttpEndpoint {
            url,
            auth_header_env,
        } => Box::new(HttpImageEndpointProcessor::from_env(
            url,
            auth_header_env.as_deref(),
        )?),
    };
    Ok(Some(processor))
}

fn image_warranted(summary: &FeedSummary, config: &ImageConfig) -> bool {
    if config.decision == ImageDecisionMode::AlwaysAsk {
        return true;
    }
    summary.score >= 75
        && matches!(
            summary.story_family,
            StoryFamily::Turn
                | StoryFamily::Test
                | StoryFamily::Permission
                | StoryFamily::FileChange
                | StoryFamily::Incident
                | StoryFamily::IdleRecap
        )
}

fn deterministic_output(request: &SummaryRequest) -> ProcessorSummary {
    if request.stories.len() == 1 {
        let story = &request.stories[0];
        return ProcessorSummary {
            headline: story.headline.clone(),
            deck: story.deck.clone(),
            lower_third: Some(strict_lower_third(
                story.agent.as_str(),
                story.project.as_deref(),
                story.family,
                story.score,
            )),
            chips: story.chips.clone(),
            publish: None,
            publish_reason: None,
            memory_digest: None,
            semantic_fingerprint: None,
        };
    }

    if let Some(story) = representative_story(request) {
        return ProcessorSummary {
            headline: story.headline.clone(),
            deck: story.deck.clone(),
            lower_third: Some(strict_lower_third(
                story.agent.as_str(),
                story.project.as_deref(),
                story.family,
                story.score,
            )),
            chips: story.chips.clone(),
            publish: None,
            publish_reason: None,
            memory_digest: None,
            semantic_fingerprint: None,
        };
    }

    let input_count = request.stories.len();
    let max_score = request
        .stories
        .iter()
        .map(|story| story.score)
        .max()
        .unwrap_or_default();
    let incidents = request
        .stories
        .iter()
        .filter(|story| {
            matches!(
                story.family,
                StoryFamily::Incident | StoryFamily::Permission
            )
        })
        .count();
    let files = request
        .stories
        .iter()
        .filter(|story| story.family == StoryFamily::FileChange)
        .count();
    let tests = request
        .stories
        .iter()
        .filter(|story| story.family == StoryFamily::Test)
        .count();

    let agent_label = agent_label(&request.stories);
    let mut facts = vec![format!("{input_count} settled stories")];
    if incidents > 0 {
        facts.push(format!("{incidents} incidents"));
    }
    if files > 0 {
        facts.push(format!("{files} file-change stories"));
    }
    if tests > 0 {
        facts.push(format!("{tests} test signals"));
    }

    ProcessorSummary {
        headline: fallback_headline(request),
        deck: format!("{}.", facts.join(". ")),
        lower_third: Some(format!(
            "{agent_label} · feed-rollup · score {max_score} · redacted"
        )),
        chips: vec![
            agent_label,
            "feed-rollup".to_string(),
            format!("{input_count} stories"),
            format!("score {max_score}"),
            "redacted".to_string(),
        ],
        publish: None,
        publish_reason: None,
        memory_digest: None,
        semantic_fingerprint: None,
    }
}

fn representative_story(request: &SummaryRequest) -> Option<&CompiledStory> {
    request
        .stories
        .iter()
        .rev()
        .filter(|story| story_has_public_outcome(story))
        .max_by_key(|story| story.score)
}

fn story_has_public_outcome(story: &CompiledStory) -> bool {
    let headline = normalize_text(&story.headline);
    let deck = normalize_text(&story.deck);
    let combined = format!("{headline} {deck}");
    !combined.trim().is_empty()
        && !public_copy_has_banned_terms(&combined)
        && !is_operational_status_without_public_impact(&combined)
        && !is_file_count_without_public_impact(&combined)
        && !is_test_status_without_public_impact(&combined)
        && !is_agent_activity_without_public_impact(&headline, &combined)
        && has_public_outcome_context(&combined)
}

fn build_summary(
    request: &SummaryRequest,
    processor_output: ProcessorSummary,
    config: &SummaryConfig,
) -> Result<FeedSummary, SummaryError> {
    let budget = &config.budget;
    let guardrails = &config.guardrails;
    let processor_publish = processor_output.publish;
    let processor_publish_reason = processor_output.publish_reason.clone();
    let project_tags = project_tags_for_stories(&request.stories, guardrails);
    let (mut headline, mut violations) = clean_and_clamp_with_project_tags(
        &processor_output.headline,
        budget.max_headline_chars,
        guardrails,
        &project_tags,
    )?;
    headline = remove_project_placeholder_inline(&headline);
    if headline.is_empty() {
        let (fallback, fallback_violations) = clean_and_clamp_with_project_tags(
            &fallback_headline(request),
            budget.max_headline_chars,
            guardrails,
            &project_tags,
        )?;
        headline = remove_project_placeholder_inline(&fallback);
        violations.extend(fallback_violations);
    }
    if headline.is_empty() {
        headline = "feed activity settled".to_string();
    }

    let (mut deck, deck_violations) = clean_and_clamp_with_project_tags(
        &processor_output.deck,
        budget.max_deck_chars,
        guardrails,
        &project_tags,
    )?;
    deck = remove_project_placeholder_inline(&deck);
    violations.extend(deck_violations);
    if deck.is_empty() {
        let (fallback, fallback_violations) = clean_and_clamp_with_project_tags(
            &fallback_deck(request),
            budget.max_deck_chars,
            guardrails,
            &project_tags,
        )?;
        deck = remove_project_placeholder_inline(&fallback);
        violations.extend(fallback_violations);
    }
    if deck.is_empty() {
        deck = "settled story activity reached the feed.".to_string();
    }

    let (mut lower_third, lower_violations) = clean_and_clamp_with_project_tags(
        processor_output
            .lower_third
            .as_deref()
            .unwrap_or("feed · redacted"),
        budget.max_lower_third_chars,
        guardrails,
        &project_tags,
    )?;
    violations.extend(lower_violations);
    if lower_third.is_empty() {
        lower_third = "feed · redacted".to_string();
    }
    lower_third = remove_project_placeholder_segments(&lower_third);
    if let Some(project) = project_tags.first() {
        lower_third = prepend_project_to_lower_third(&lower_third, project);
        lower_third = clamp_chars(&lower_third, budget.max_lower_third_chars);
    }

    let mut chips = project_chips(&project_tags, budget.max_chips);
    for chip in processor_output.chips.into_iter().take(budget.max_chips) {
        let (chip, chip_violations) =
            clean_chip(&chip, budget.max_chip_chars, guardrails, &project_tags)?;
        violations.extend(chip_violations);
        if !chip.is_empty() && !chips.iter().any(|existing| existing == &chip) {
            chips.push(chip);
        }
        if chips.len() >= budget.max_chips {
            break;
        }
    }
    if chips.is_empty() {
        chips.push("redacted".to_string());
    }

    let story_family = summary_family(&request.stories);
    let severity = max_severity(&request.stories);
    let score = request
        .stories
        .iter()
        .map(|story| story.score)
        .max()
        .unwrap_or_default();
    let source_agent_kinds = request
        .stories
        .iter()
        .map(|story| story.agent.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let evidence_event_ids = request
        .stories
        .iter()
        .flat_map(|story| story.evidence_event_ids.iter().cloned())
        .take(budget.max_evidence_items)
        .collect::<Vec<_>>();
    let story_window = if request.stories.len() == 1 {
        request.stories[0]
            .key
            .turn_id
            .clone()
            .or_else(|| request.stories[0].key.session_id.clone())
            .unwrap_or_else(|| "window".to_string())
    } else {
        "feed-rollup".to_string()
    };

    let headline_fingerprint = headline_fingerprint(&headline);
    let mut summary = FeedSummary {
        story_window,
        source_agent_kinds,
        headline,
        deck,
        lower_third,
        chips,
        image: None,
        story_family,
        severity,
        score,
        privacy_class: PrivacyClass::Redacted,
        evidence_event_ids,
        metadata: SummaryMetadata {
            processor: config.processor.name().to_string(),
            policy: guardrails.name.clone(),
            image_processor: config.image.processor.name().to_string(),
            publish_action: PublishAction::Publish,
            publish_reason: processor_publish_reason
                .unwrap_or_else(|| "local publish policy accepted the summary".to_string()),
            headline_fingerprint,
            max_headline_similarity: 0,
            max_deck_similarity: 0,
            guardrail_version: guardrails.version,
            input_stories: request.stories.len(),
            output_chars: 0,
            external_cost_allowed: !matches!(
                config.processor,
                SummaryProcessorConfig::Deterministic
            ),
            image_enabled: config.image.enabled,
            violations,
        },
    };
    polish_public_summary(&mut summary);
    fit_capsule_budget(&mut summary, budget, guardrails)?;
    if processor_publish == Some(false)
        && config.publish.allow_processor_skip
        && summary.score < config.publish.severe_score_bypass
    {
        summary.metadata.publish_action = PublishAction::SkipProcessor;
        if summary.metadata.publish_reason.trim().is_empty()
            || summary.metadata.publish_reason == "local publish policy accepted the summary"
        {
            summary.metadata.publish_reason =
                "processor reported no meaningful feed change".to_string();
        }
    }
    summary.metadata.output_chars =
        summary.headline.len() + summary.deck.len() + summary.lower_third.len();
    Ok(summary)
}

fn clean_and_clamp(
    value: &str,
    max_chars: usize,
    guardrails: &SummaryGuardrails,
) -> Result<(String, Vec<GuardrailViolation>), SummaryError> {
    clean_and_clamp_with_project_tags(value, max_chars, guardrails, &[])
}

fn clean_and_clamp_with_project_tags(
    value: &str,
    max_chars: usize,
    guardrails: &SummaryGuardrails,
    project_tags: &[String],
) -> Result<(String, Vec<GuardrailViolation>), SummaryError> {
    let mut input = value.to_string();
    if !guardrails.allow_project_names {
        input = mask_project_like_terms_except(
            &input,
            if guardrails.allow_project_tags {
                project_tags
            } else {
                &[]
            },
        );
    }
    if !guardrails.allow_command_text {
        input = mask_command_like_terms(&input);
    }
    let (input, mut violations) = strip_prompt_leakage(&input)?;
    let (cleaned, guardrail_violations) = guardrails.clean_text(&input)?;
    violations.extend(guardrail_violations);
    Ok((clamp_chars(&cleaned, max_chars), violations))
}

fn clean_chip(
    value: &str,
    max_chars: usize,
    guardrails: &SummaryGuardrails,
    project_tags: &[String],
) -> Result<(String, Vec<GuardrailViolation>), SummaryError> {
    if guardrails.allow_project_tags
        && project_tags
            .iter()
            .any(|project| project.eq_ignore_ascii_case(value.trim()))
    {
        return Ok((clamp_chars(value.trim(), max_chars), Vec::new()));
    }
    clean_and_clamp(value, max_chars, guardrails)
        .map(|(chip, violations)| (remove_project_placeholder_segments(&chip), violations))
}

fn project_tags_for_stories(
    stories: &[CompiledStory],
    guardrails: &SummaryGuardrails,
) -> Vec<String> {
    if !guardrails.allow_project_tags {
        return Vec::new();
    }
    stories
        .iter()
        .filter_map(|story| story.project.as_deref().and_then(public_project_tag))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn project_chips(project_tags: &[String], max_chips: usize) -> Vec<String> {
    if max_chips == 0 || project_tags.is_empty() {
        return Vec::new();
    }
    if project_tags.len() <= 3 {
        return project_tags.iter().take(max_chips).cloned().collect();
    }
    let mut chips = project_tags
        .iter()
        .take(max_chips.saturating_sub(1).min(2))
        .cloned()
        .collect::<Vec<_>>();
    if chips.len() < max_chips {
        chips.push(format!("{} projects", project_tags.len()));
    }
    chips
}

fn prepend_project_to_lower_third(lower_third: &str, project: &str) -> String {
    let lower = lower_third.trim();
    if lower
        .split('·')
        .map(str::trim)
        .any(|part| part.eq_ignore_ascii_case(project))
    {
        lower.to_string()
    } else if lower.is_empty() {
        project.to_string()
    } else {
        format!("{project} · {lower}")
    }
}

fn remove_project_placeholder_segments(value: &str) -> String {
    let cleaned = value
        .split('·')
        .map(str::trim)
        .filter(|part| !part.eq_ignore_ascii_case("[project]"))
        .collect::<Vec<_>>()
        .join(" · ");
    if cleaned.is_empty() && value.trim().eq_ignore_ascii_case("[project]") {
        String::new()
    } else {
        cleaned
    }
}

fn remove_project_placeholder_inline(value: &str) -> String {
    Regex::new(r"(?i)(?:^|\s+)\[project\](?:\s+|$)")
        .expect("project placeholder regex is valid")
        .replace_all(value, " ")
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(['.', ',', ';', ':', '-'])
        .trim()
        .to_string()
}

fn public_project_tag(project: &str) -> Option<String> {
    let project = project.trim().trim_matches(['.', ',', ';', ':']);
    if project.len() < 2 || project.len() > 40 {
        return None;
    }
    if project.contains('/') || project.contains('\\') || project.ends_with(".rs") {
        return None;
    }
    if !project
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return None;
    }
    let normalized = project.to_ascii_lowercase();
    if [
        "secret",
        "secrets",
        "private",
        "token",
        "tokens",
        "credential",
        "credentials",
        "password",
        "passwd",
        "key",
        "keys",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return None;
    }
    Some(project.to_string())
}

fn fit_capsule_budget(
    summary: &mut FeedSummary,
    budget: &SummaryBudget,
    guardrails: &SummaryGuardrails,
) -> Result<(), SummaryError> {
    while summary.headline.len() + summary.deck.len() + summary.lower_third.len()
        > budget.max_capsule_chars
    {
        if summary.deck.len() <= 48 {
            break;
        }
        summary.deck = clamp_chars(&summary.deck, summary.deck.len().saturating_sub(24));
    }
    let (deck, violations) = clean_and_clamp(&summary.deck, budget.max_deck_chars, guardrails)?;
    summary.metadata.violations.extend(violations);
    summary.deck = deck;
    if summary.deck.is_empty() {
        summary.deck = "settled story activity reached the feed.".to_string();
    }
    Ok(())
}

fn polish_public_summary(summary: &mut FeedSummary) {
    if let Some((headline, deck)) = impact_rewrite(&summary.headline, &summary.deck) {
        summary.headline = headline;
        summary.deck = deck;
    }
    summary.headline = remove_agent_headline_prefix(&polish_public_copy(&summary.headline));
    summary.deck = ensure_terminal_sentence(&polish_public_copy(&summary.deck));
    summary.lower_third = polish_public_copy(&summary.lower_third);
    summary.chips = summary
        .chips
        .iter()
        .map(|chip| polish_public_copy(chip))
        .filter(|chip| !chip.trim().is_empty())
        .fold(Vec::<String>::new(), |mut chips, chip| {
            if !chips.iter().any(|existing| existing == &chip) {
                chips.push(chip);
            }
            chips
        });
    if summary.chips.is_empty() {
        summary.chips.push("redacted".to_string());
    }
}

fn impact_rewrite(headline: &str, deck: &str) -> Option<(String, String)> {
    let combined = normalize_text(&format!("{headline} {deck}"));
    let rewrite = if combined.contains("real codex stream")
        || combined.contains("settled summarization")
        || combined.contains("p2p capsule")
        || combined.contains("story capsule")
    {
        Some((
            "feed turns live agent work into safer public story capsules",
            "local activity is aggregated into settled, redacted headlines before it reaches the network",
        ))
    } else if combined.contains("github username")
        || (combined.contains("github") && combined.contains("discovery"))
    {
        Some((
            "github profile routes open verified public feed streams",
            "viewer urls resolve durable github identity before subscribing to visible stories",
        ))
    } else if combined.contains("publisher identity")
        || (combined.contains("verified") && combined.contains("publisher"))
        || combined.contains("verified feed authors")
    {
        Some((
            "remote headlines carry verified publisher identity",
            "browser viewers can see the github account behind each feed story",
        ))
    } else if combined.contains("headline image") || combined.contains("media layer") {
        Some((
            "publishers can attach opt-in visuals to major feed stories",
            "image generation stays disabled by default and runs behind feed guardrails",
        ))
    } else if combined.contains("org level") || combined.contains("organization") {
        Some((
            "organization feeds can gate discovery by github membership",
            "teams can publish shared feeds without opening every story to the public network",
        ))
    } else if combined.contains("sign in") || combined.contains("oauth") {
        Some((
            "cli publishing now binds feeds to github sign-in",
            "native publishers can prove account identity before sending stories to the network",
        ))
    } else if combined.contains("fabric") && combined.contains("subscription") {
        Some((
            "network routing stays separate from explicit feed subscriptions",
            "peers can support discovery and browser handoff without auto-following private feeds",
        ))
    } else if combined.contains("publish gating") || combined.contains("story quality") {
        Some((
            "feed suppresses duplicate mechanics before publishing",
            "the story compiler now favors outcome changes over command, file, and test chatter",
        ))
    } else {
        None
    }?;
    Some((rewrite.0.to_string(), rewrite.1.to_string()))
}

fn polish_public_copy(input: &str) -> String {
    let mut output = input.trim().to_string();
    for (pattern, replacement) in PUBLIC_COPY_REPLACEMENTS {
        output = Regex::new(pattern)
            .expect("public copy replacement pattern is valid")
            .replace_all(&output, *replacement)
            .to_string();
    }
    output = Regex::new(r"\s+")
        .expect("space regex is valid")
        .replace_all(&output, " ")
        .trim()
        .trim_matches(['.', ',', ';', ':', '-'])
        .trim()
        .to_string();
    output = output
        .replace(" ;", ";")
        .replace("; ;", ";")
        .replace(" .", ".")
        .replace(" ,", ",");
    lower_known_agent_prefix(&output)
}

const PUBLIC_COPY_REPLACEMENTS: &[(&str, &str)] = &[
    (r"(?i)\bproduction scaffold\b", "feed implementation"),
    (r"(?i)\bproduction flow\b", "feed publishing"),
    (r"(?i)\bagent feed scaffold\b", "agent feed implementation"),
    (r"(?i)\bagent feed work\b", "agent feed"),
    (r"(?i)\bscaffold\b", "implementation"),
    (r"(?i)\bfixture-driven\b", "mock"),
    (r"(?i)\bfixture events\b", "mock events"),
    (r"(?i)\bfixture\b", "mock"),
    (r"(?i)\bm[0-9]+(?:\.[0-9]+)?\s+signal path\b", "signal path"),
    (r"(?i)\bm[0-9]+(?:\.[0-9]+)?\b", ""),
    (r"(?i)\btest gate stays red\b", "tests are still failing"),
    (r"(?i)\btest line remains red\b", "tests are still failing"),
    (r"(?i)\btests?\s+remain\s+red\b", "tests are still failing"),
    (
        r"(?i)\btests?\s+remain\s+failing\b",
        "tests are still failing",
    ),
    (r"(?i)\btests?\s+red\b", "tests failing"),
    (r"(?i)\blatest\s+red\s+runs?\b", "latest failing run"),
    (r"(?i)\bred\s+runs?\b", "failing runs"),
    (r"(?i)\bfailures\s+remain\b", "follow-up remains"),
    (r"(?i)\bcurrent public outcome remains\b", ""),
    (r"(?i)\bchanged work\b", "changed"),
    (r"(?i)\bmoved forward\b", "changed"),
    (r"(?i)\badvanced across the feed\b", "changed"),
    (r"(?i)\bcoverage advanced\b", "coverage landed"),
    (r"(?i)\badvances\b", "moves"),
    (r"(?i)\badvanced\b", "landed"),
    (
        r"(?i)\bclosed the summarization pass\b",
        "finished summarization",
    ),
    (r"(?i)\blater verified passing\b", "verified passing"),
    (r"(?i)\bearlier red runs\b", "earlier failing runs"),
    (r"(?i)\bred runs\b", "failing runs"),
    (r"(?i)\bresponse-body\b", "response body"),
    (r"(?i)\bverification s\b", "verification"),
    (
        r"(?i)\bcommand lifecycle captured without command output\b",
        "safe shell activity settled",
    ),
    (
        r"(?i)\bcommand lifecycle captured\b",
        "safe shell activity settled",
    ),
    (r"(?i)\bwithout command output\b", ""),
];

fn ensure_terminal_sentence(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.ends_with(['.', '!', '?']) {
        return trimmed.to_string();
    }
    format!("{trimmed}.")
}

fn lower_known_agent_prefix(input: &str) -> String {
    for agent in ["Codex", "Claude"] {
        if let Some(rest) = input.strip_prefix(agent) {
            return format!("{}{}", agent.to_ascii_lowercase(), rest);
        }
    }
    input.to_string()
}

fn remove_agent_headline_prefix(input: &str) -> String {
    let trimmed = input.trim();
    let lowered = trimmed.to_ascii_lowercase();
    for prefix in ["codex ", "claude ", "agent "] {
        if lowered.starts_with(prefix) {
            return lower_initial(trimmed[prefix.len()..].trim_start());
        }
    }
    trimmed.to_string()
}

fn lower_initial(input: &str) -> String {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_lowercase().chain(chars).collect()
}

fn strip_prompt_leakage(input: &str) -> Result<(String, Vec<GuardrailViolation>), SummaryError> {
    let mut output = input.to_string();
    let mut changed = false;
    for pattern in PROMPT_LEAKAGE_PATTERNS {
        let regex = Regex::new(pattern).map_err(|err| {
            SummaryError::Processor(format!("invalid prompt-leakage pattern: {err}"))
        })?;
        if regex.is_match(&output) {
            output = regex.replace_all(&output, " ").to_string();
            changed = true;
        }
    }
    output = tidy_display_text(&output);
    let violations = changed
        .then(|| GuardrailViolation {
            name: PROMPT_LEAKAGE_VIOLATION.to_string(),
            action: GuardrailAction::Mask,
        })
        .into_iter()
        .collect();
    Ok((output, violations))
}

fn tidy_display_text(input: &str) -> String {
    let mut output = input.split_whitespace().collect::<Vec<_>>().join(" ");
    for (from, to) in [
        (" .", "."),
        (" ,", ","),
        (" ;", ";"),
        (" :", ":"),
        ("..", "."),
        (". .", "."),
        ("· ·", "·"),
    ] {
        while output.contains(from) {
            output = output.replace(from, to);
        }
    }
    output = output
        .trim_matches(|ch: char| ch.is_whitespace() || matches!(ch, '·' | '-' | ',' | ';' | ':'))
        .to_string();
    if output.chars().any(|ch| ch.is_alphanumeric()) {
        output
    } else {
        String::new()
    }
}

fn fallback_headline(request: &SummaryRequest) -> String {
    if request.stories.len() == 1 {
        let story = &request.stories[0];
        format!("{} {} settled", story.agent, family_display(story.family))
    } else {
        format!("{} feed activity settled", agent_label(&request.stories))
    }
}

fn fallback_deck(request: &SummaryRequest) -> String {
    if request.stories.len() == 1 {
        let story = &request.stories[0];
        format!(
            "one {} story reached the feed.",
            family_display(story.family)
        )
    } else {
        format!(
            "{} settled stories reached the feed.",
            request.stories.len()
        )
    }
}

fn agent_label(stories: &[CompiledStory]) -> String {
    let agents = stories
        .iter()
        .map(|story| story.agent.clone())
        .collect::<BTreeSet<_>>();
    if agents.len() == 1 {
        agents
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "agents".to_string())
    } else {
        format!("{} agents", agents.len())
    }
}

fn family_display(family: StoryFamily) -> &'static str {
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

fn apply_publish_decision(
    summary: &mut FeedSummary,
    request: &SummaryRequest,
    policy: &PublishDecisionConfig,
) -> bool {
    if !policy.enabled {
        summary.metadata.publish_action = PublishAction::Publish;
        summary.metadata.publish_reason = "publish policy disabled".to_string();
        return true;
    }

    if summary.metadata.publish_action == PublishAction::SkipProcessor {
        return false;
    }

    let mut nearest_headline = 0u8;
    let mut nearest_deck = 0u8;
    let mut nearest = None::<&RecentSummary>;
    for recent in request
        .recent_summaries
        .iter()
        .take(policy.recent_window.max(1))
    {
        if recent.story_family != summary.story_family {
            continue;
        }
        let headline_score = headline_similarity(&summary.headline, &recent.headline);
        let deck_score = text_similarity(&summary.deck, &recent.deck);
        if headline_score > nearest_headline
            || (headline_score == nearest_headline && deck_score > nearest_deck)
        {
            nearest_headline = headline_score;
            nearest_deck = deck_score;
            nearest = Some(recent);
        }
    }

    summary.metadata.max_headline_similarity = nearest_headline;
    summary.metadata.max_deck_similarity = nearest_deck;
    summary.metadata.headline_fingerprint = headline_fingerprint(&summary.headline);

    if is_generic_or_low_signal_summary(summary) {
        summary.metadata.publish_action = PublishAction::SkipProcessor;
        summary.metadata.publish_reason =
            "summary quality gate rejected a generic low-context headline".to_string();
        return false;
    }

    let duplicate = nearest.is_some()
        && nearest_headline >= policy.max_headline_similarity
        && (nearest_headline == 100
            || nearest_deck >= policy.max_deck_similarity_when_headline_matches);
    if duplicate {
        summary.metadata.publish_action = PublishAction::SkipDuplicate;
        summary.metadata.publish_reason = format!(
            "headline did not meaningfully change from a recent published summary (headline similarity {nearest_headline}, deck similarity {nearest_deck})"
        );
        return false;
    }

    if summary.score >= policy.severe_score_bypass {
        summary.metadata.publish_action = PublishAction::Publish;
        summary.metadata.publish_reason =
            "high-severity summary bypassed duplicate suppression".to_string();
        return true;
    }

    summary.metadata.publish_action = PublishAction::Publish;
    summary.metadata.publish_reason = if nearest.is_some() {
        format!(
            "headline changed enough to publish (nearest headline similarity {nearest_headline}, deck similarity {nearest_deck})"
        )
    } else {
        "no recent matching feed summary".to_string()
    };
    true
}

fn is_generic_or_low_signal_summary(summary: &FeedSummary) -> bool {
    let headline = normalize_text(&summary.headline);
    let deck = normalize_text(&summary.deck);
    let combined = format!("{headline} {deck}");
    if public_copy_has_banned_terms(&combined) {
        return true;
    }
    [
        "feed activity settled",
        "activity settled",
        "shell command failed",
        "hit project command",
        "hit command",
        "tool failures",
        "settled stories",
    ]
    .iter()
    .any(|needle| combined.contains(needle))
        || is_operational_status_without_public_impact(&combined)
        || is_file_count_without_public_impact(&combined)
        || is_test_status_without_public_impact(&combined)
        || is_agent_activity_without_public_impact(&headline, &combined)
        || (summary.story_family == StoryFamily::FileChange
            && headline.contains("changed")
            && headline.contains("file")
            && deck.contains("changed")
            && deck.contains("file")
            && !combined.contains("test")
            && !combined.contains("verified")
            && !combined.contains("complete"))
}

fn is_operational_status_without_public_impact(normalized: &str) -> bool {
    [
        "ci status",
        "run state",
        "command event",
        "command events",
        "plan state",
        "plan-state",
        "plan update",
        "safe command",
        "shell check",
        "shell checks",
        "repository state",
        "prior ci status",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
        && !has_public_outcome_context(normalized)
}

fn is_file_count_without_public_impact(normalized: &str) -> bool {
    let has_file_count = Regex::new(
        r"\b(?:[0-9]+|one|two|three|four|five|six|seven|eight|nine|ten)\s+(?:changed\s+)?files?\b|\bfiles?\s+changed\b",
    )
    .expect("file-count regex is valid")
    .is_match(normalized);
    has_file_count && !has_public_outcome_context(normalized)
}

fn is_test_status_without_public_impact(normalized: &str) -> bool {
    [
        "tests passed",
        "test passed",
        "tests verified",
        "verified tests",
        "confirms pass state",
        "pass state",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
        && !has_public_outcome_context(normalized)
}

fn is_agent_activity_without_public_impact(headline: &str, combined: &str) -> bool {
    (headline.starts_with("codex ")
        || headline.starts_with("claude ")
        || headline.starts_with("agent "))
        && !has_public_outcome_context(combined)
}

fn has_public_outcome_context(normalized: &str) -> bool {
    [
        "auth",
        "avatar",
        "browser",
        "broadcast",
        "capture",
        "callback",
        "deployment",
        "discovery",
        "edge",
        "github",
        "guardrail",
        "install",
        "network",
        "open source",
        "package",
        "privacy",
        "public",
        "publish",
        "release",
        "route",
        "security",
        "ship",
        "shipped",
        "stream",
        "subscription",
        "summarization",
        "summary",
        "update",
        "user",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn public_copy_has_banned_terms(normalized: &str) -> bool {
    [
        "production scaffold",
        "production flow",
        "agent feed scaffold",
        "checks ci status",
        "completed turn",
        "file-change pass",
        "planning state advanced",
        "plan update",
        "records plan update",
        "run state settled",
        "test gate",
        "test line",
        "two-file update",
        "fixture-driven",
        "fixture events",
        "moved forward",
        "advanced across the feed",
        "verification s",
        "shifts feed to edits",
        "settles run state",
        "codexci statusrun state",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
        || Regex::new(r"\bm[0-9]+(?:\.[0-9]+)?\b")
            .expect("milestone regex is valid")
            .is_match(normalized)
}

#[must_use]
pub fn headline_similarity(left: &str, right: &str) -> u8 {
    text_similarity(left, right)
}

#[must_use]
pub fn headline_fingerprint(headline: &str) -> String {
    let terms = meaningful_terms(headline);
    if terms.is_empty() {
        normalize_text(headline)
    } else {
        terms.into_iter().collect::<Vec<_>>().join(":")
    }
}

fn text_similarity(left: &str, right: &str) -> u8 {
    let left_normalized = normalize_text(left);
    let right_normalized = normalize_text(right);
    if left_normalized.is_empty() || right_normalized.is_empty() {
        return 0;
    }
    if left_normalized == right_normalized {
        return 100;
    }

    let left_terms = meaningful_terms(left);
    let right_terms = meaningful_terms(right);
    if left_terms.is_empty() || right_terms.is_empty() {
        return 0;
    }
    let intersection = left_terms.intersection(&right_terms).count();
    let union = left_terms.union(&right_terms).count();
    ((intersection * 100) / union).min(100) as u8
}

fn meaningful_terms(input: &str) -> BTreeSet<String> {
    normalize_text(input)
        .split_whitespace()
        .filter(|word| !is_similarity_stopword(word))
        .filter(|word| !word.chars().all(|ch| ch.is_ascii_digit()))
        .map(|word| {
            word.strip_suffix('s')
                .filter(|stem| stem.len() > 3)
                .unwrap_or(word)
                .to_string()
        })
        .collect()
}

fn normalize_text(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
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

fn is_similarity_stopword(word: &str) -> bool {
    matches!(
        word,
        "a" | "an"
            | "and"
            | "agent"
            | "claude"
            | "codex"
            | "detail"
            | "feed"
            | "for"
            | "from"
            | "in"
            | "local"
            | "of"
            | "omitted"
            | "on"
            | "raw"
            | "redacted"
            | "signal"
            | "the"
            | "to"
            | "with"
    )
}

fn processor_prompt(request: &SummaryRequest) -> String {
    let style = request
        .prompt_style
        .as_deref()
        .map(str::trim)
        .filter(|style| !style.is_empty())
        .unwrap_or(DEFAULT_SUMMARY_PROMPT_STYLE);
    let max_prompt_chars = request
        .max_prompt_chars
        .unwrap_or(DEFAULT_SUMMARY_PROMPT_MAX_CHARS)
        .max(512);
    let stories = request
        .stories
        .iter()
        .map(|story| {
            format!(
                "- project={} agent={} family={:?} score={} headline={} deck={}",
                story.project.as_deref().unwrap_or("none"),
                story.agent,
                story.family,
                story.score,
                story.headline,
                story.deck
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let recent = request
        .recent_summaries
        .iter()
        .map(|summary| {
            format!(
                "- family={:?} score={} headline={} deck={}",
                summary.story_family, summary.score, summary.headline, summary.deck
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let recent = if recent.is_empty() {
        "recent_published=none".to_string()
    } else {
        format!("recent_published:\n{recent}")
    };
    let prompt = format!(
        "{INTERNAL_SUMMARIZER_MARKER}\nReturn one JSON object with headline, deck, lower_third, chips, and optional publish/publish_reason. Set publish=false when the candidate is not meaningfully different from recent published summaries, or when the facts only show agent mechanics such as CI polling, command bursts, file counts, plan state, repository status, or test status without a clear product/work outcome. Write with this style: {style}. The headline should read like compact news: what shipped, improved, regressed, or became useful, and why it matters to users, operators, or open-source consumers. Do not start the headline with Codex, Claude, or an agent name. Keep provided project labels in chips or lower_third for multi-thread tracking; do not force them into headline or deck. Use only the redacted story facts below. Do not include raw prompts, command output, diffs, absolute paths, repo names beyond provided project labels, emails, secrets, tokens, credentials, personal data, or policy/omission copy.\nfeed={}\nmode={:?}\n{}\nstories:\n{}",
        request.feed_id, request.mode, recent, stories
    );
    clamp_chars(&prompt, max_prompt_chars)
}

fn image_processor_prompt(request: &ImageRequest) -> String {
    let base = format!(
        "{INTERNAL_SUMMARIZER_MARKER}\nReturn one JSON object. Either return {{\"image\": null, \"reason\": \"...\"}} when no useful projection-safe image exists, or return {{\"image\": {{\"uri\": \"...\", \"alt\": \"...\", \"source\": \"generated\"}}}}. Generate or reference only display-safe imagery. Do not include raw prompts, readable code, command output, diffs, secrets, tokens, credentials, exact paths, repo names, emails, or personal data. Use this visual style: {}.\nheadline={}\ndeck={}\nlower_third={}\nchips={}\nfamily={:?}\nscore={}",
        request.policy.prompt_style,
        request.headline,
        request.deck,
        request.lower_third,
        request.chips.join(", "),
        request.story_family,
        request.score
    );
    clamp_chars(&base, request.policy.max_prompt_chars)
}

struct ParsedProcessorOutput {
    summary: ProcessorSummary,
    codex_session_id: Option<String>,
}

fn parse_processor_output(output: &str) -> Result<ProcessorSummary, SummaryError> {
    parse_processor_output_with_meta(output).map(|parsed| parsed.summary)
}

fn parse_processor_output_with_meta(output: &str) -> Result<ParsedProcessorOutput, SummaryError> {
    if let Ok(summary) = serde_json::from_str::<ProcessorSummary>(output) {
        return Ok(ParsedProcessorOutput {
            summary,
            codex_session_id: None,
        });
    }

    let mut codex_session_id = None;
    let mut last_message = None::<String>;
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("session_meta") {
            codex_session_id = value
                .get("payload")
                .and_then(|payload| payload.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .or(codex_session_id);
        }
        if value.get("type").and_then(serde_json::Value::as_str) == Some("thread.started") {
            codex_session_id = value
                .get("thread_id")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .or(codex_session_id);
        }
        if let Some(message) = codex_message_text(&value) {
            last_message = Some(message);
        }
    }
    if let Some(message) = last_message {
        if let Some(json) = extract_json_object(&message)
            && let Ok(summary) = serde_json::from_str::<ProcessorSummary>(json)
        {
            return Ok(ParsedProcessorOutput {
                summary,
                codex_session_id,
            });
        }
        return Ok(ParsedProcessorOutput {
            summary: ProcessorSummary {
                headline: "feed activity settled".to_string(),
                deck: clamp_chars(message.trim(), 220),
                lower_third: Some("external processor · redacted".to_string()),
                chips: vec!["external".to_string(), "redacted".to_string()],
                publish: None,
                publish_reason: Some("processor returned prose instead of json".to_string()),
                memory_digest: None,
                semantic_fingerprint: None,
            },
            codex_session_id,
        });
    }

    Ok(ParsedProcessorOutput {
        summary: ProcessorSummary {
            headline: "feed activity settled".to_string(),
            deck: clamp_chars(output.trim(), 220),
            lower_third: Some("external processor · redacted".to_string()),
            chips: vec!["external".to_string(), "redacted".to_string()],
            publish: None,
            publish_reason: Some("processor output did not include a final message".to_string()),
            memory_digest: None,
            semantic_fingerprint: None,
        },
        codex_session_id,
    })
}

fn codex_message_text(value: &serde_json::Value) -> Option<String> {
    if value.get("type").and_then(serde_json::Value::as_str) == Some("item.completed") {
        let item = value.get("item")?;
        if item.get("type").and_then(serde_json::Value::as_str) == Some("agent_message") {
            return item
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
        }
    }

    let payload = value.get("payload")?;
    if value.get("type").and_then(serde_json::Value::as_str) == Some("event_msg")
        && payload.get("type").and_then(serde_json::Value::as_str) == Some("task_complete")
    {
        return payload
            .get("last_agent_message")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
    }
    if value.get("type").and_then(serde_json::Value::as_str) != Some("response_item")
        || payload.get("type").and_then(serde_json::Value::as_str) != Some("message")
        || payload.get("role").and_then(serde_json::Value::as_str) != Some("assistant")
    {
        return None;
    }
    let content = payload.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter_map(|item| {
            item.get("text")
                .or_else(|| item.get("content"))
                .and_then(serde_json::Value::as_str)
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn extract_json_object(input: &str) -> Option<&str> {
    let start = input.find('{')?;
    let end = input.rfind('}')?;
    (end > start).then_some(&input[start..=end])
}

fn parse_image_processor_output(
    output: &str,
    config: &ImageConfig,
    processor_name: &str,
) -> Result<Option<HeadlineImage>, SummaryError> {
    let response = serde_json::from_str::<ProcessorImageResponse>(output).map_err(|err| {
        SummaryError::Processor(format!("image processor returned invalid json: {err}"))
    })?;
    let Some(image) = response.image else {
        return Ok(None);
    };
    let source = image.source.unwrap_or_else(|| processor_name.to_string());
    let candidate = HeadlineImage::new(
        clamp_chars(image.uri.trim(), 512),
        clamp_chars(image.alt.trim(), 180),
        clamp_chars(source.trim(), 80),
    );
    validate_headline_image(&candidate, config)?;
    Ok(Some(candidate))
}

fn validate_headline_image(
    image: &HeadlineImage,
    config: &ImageConfig,
) -> Result<(), SummaryError> {
    validate_image_uri(&image.uri, config)?;
    if image.alt.is_empty() {
        return Err(SummaryError::Processor(
            "image processor returned empty alt text".to_string(),
        ));
    }
    validate_image_text("alt text", &image.alt)?;
    validate_image_text("source", &image.source)
}

fn validate_image_uri(uri: &str, config: &ImageConfig) -> Result<(), SummaryError> {
    if uri.is_empty() {
        return Err(SummaryError::Processor(
            "image processor returned empty uri".to_string(),
        ));
    }
    if looks_sensitive(uri) {
        return Err(SummaryError::Processor(
            "image uri rejected by guardrails".to_string(),
        ));
    }
    if config
        .allowed_uri_prefixes
        .iter()
        .any(|prefix| uri.starts_with(prefix))
    {
        return Ok(());
    }
    if config.allow_remote_urls && uri.starts_with("https://") {
        return Ok(());
    }
    Err(SummaryError::Processor(format!(
        "image uri rejected by policy: {}",
        clamp_chars(uri, 80)
    )))
}

fn validate_image_text(label: &str, value: &str) -> Result<(), SummaryError> {
    if looks_sensitive(value) {
        return Err(SummaryError::Processor(format!(
            "image {label} rejected by guardrails"
        )));
    }
    Ok(())
}

fn looks_sensitive(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("sk-")
        || lower.contains("ghp_")
        || lower.contains("gho_")
        || lower.contains("ghu_")
        || lower.contains("ghs_")
        || lower.contains("ghr_")
        || lower.contains("akia")
        || lower.contains("-----begin")
        || lower.contains("/home/")
        || lower.contains(".env")
        || lower.contains("password")
        || lower.contains("credential")
        || lower.contains("secret")
        || lower.contains("private-key")
        || lower.contains("api_key")
        || lower.contains("api-key")
        || lower.contains("token=")
        || (value.contains('@') && value.contains('.'))
}

fn summary_family(stories: &[CompiledStory]) -> StoryFamily {
    if stories.len() > 1 {
        return StoryFamily::IdleRecap;
    }
    stories
        .first()
        .map(|story| story.family)
        .unwrap_or(StoryFamily::IdleRecap)
}

fn max_severity(stories: &[CompiledStory]) -> Severity {
    stories
        .iter()
        .map(|story| story.severity)
        .max_by_key(|severity| severity_rank(*severity))
        .unwrap_or_default()
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

fn strict_lower_third(
    agent: &str,
    project: Option<&str>,
    family: StoryFamily,
    score: u8,
) -> String {
    if let Some(project) = project.and_then(public_project_tag) {
        format!(
            "{project} · {agent} · {:?} · score {score} · redacted",
            family
        )
    } else {
        format!("{agent} · {:?} · score {score} · redacted", family)
    }
}

fn mask_project_like_terms_except(input: &str, allowed_project_tags: &[String]) -> String {
    input
        .split_whitespace()
        .map(|word| {
            if is_allowed_project_tag_word(word, allowed_project_tags) {
                word
            } else if word.contains('_') || word.contains('/') || word.ends_with(".rs") {
                "[project]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_allowed_project_tag_word(word: &str, allowed_project_tags: &[String]) -> bool {
    if allowed_project_tags.is_empty() {
        return false;
    }
    let token = word
        .trim_matches(|ch: char| ch.is_ascii_punctuation() && !matches!(ch, '_' | '-' | '.'))
        .trim_matches('`');
    allowed_project_tags
        .iter()
        .any(|project| project.eq_ignore_ascii_case(token))
}

fn mask_command_like_terms(input: &str) -> String {
    input
        .replace("/usr/bin/zsh", "shell command")
        .replace("cargo ", "test command ")
        .replace("git ", "vcs command ")
}

fn clamp_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut output = input
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    if let Some((index, _)) = output
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        && index >= max_chars.saturating_sub(3) / 2
    {
        output.truncate(index);
    }
    output = output
        .trim_end_matches(|ch: char| ch.is_whitespace() || matches!(ch, ',' | ';' | ':' | '-'))
        .to_string();
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_core::{PrivacyClass, Severity};
    use agent_feed_story::StoryKey;

    fn story(title: &str, score: u8) -> CompiledStory {
        CompiledStory {
            key: StoryKey {
                feed_id: None,
                agent: "codex".to_string(),
                project_hash: Some("secret_repo".to_string()),
                session_id: Some("session".to_string()),
                turn_id: Some(format!("turn-{score}")),
                family: StoryFamily::FileChange,
            },
            created_at: time::OffsetDateTime::now_utc(),
            family: StoryFamily::FileChange,
            agent: "codex".to_string(),
            project: Some("secret_repo".to_string()),
            headline: title.to_string(),
            deck: "1 changed files. raw diff omitted.".to_string(),
            lower_third: format!("codex · secret_repo · file-change · score {score} · redacted"),
            chips: vec![
                "codex".to_string(),
                "secret_repo".to_string(),
                "1 files".to_string(),
                "file-change".to_string(),
                format!("score {score}"),
            ],
            severity: Severity::Notice,
            score,
            context_score: 92,
            privacy: PrivacyClass::Redacted,
            evidence_event_ids: vec!["evt_test".to_string()],
        }
    }

    fn verified_story(score: u8) -> CompiledStory {
        let mut story = story("codex verified tests", score);
        story.family = StoryFamily::Test;
        story.headline = "codex verified tests".to_string();
        story.deck = "tests passed after the update.".to_string();
        story
    }

    fn public_project_story(project: &str, score: u8) -> CompiledStory {
        let mut story = verified_story(score);
        story.key.project_hash = Some(project.to_string());
        story.project = Some(project.to_string());
        story.lower_third = format!("codex · {project} · test · score {score} · redacted");
        story.chips = vec![
            "codex".to_string(),
            project.to_string(),
            "test".to_string(),
            format!("score {score}"),
            "redacted".to_string(),
        ];
        story
    }

    #[test]
    fn clamp_chars_uses_word_boundary_for_public_copy() {
        let clamped = clamp_chars(
            "tests still report failures despite one passing verification suite",
            64,
        );

        assert_eq!(
            clamped,
            "tests still report failures despite one passing verification..."
        );
        assert!(!clamped.contains("verification s"));
    }

    #[test]
    fn deterministic_rollup_skips_count_only_summary() {
        let stories = (0..12)
            .map(|index| story(&format!("codex changed secret_repo {index}"), 78))
            .collect::<Vec<_>>();
        let summaries =
            summarize_feed("local:workstation", &stories, &SummaryConfig::p2p_default())
                .expect("summary compiles");

        assert!(summaries.is_empty());
    }

    #[test]
    fn deterministic_rollup_uses_meaningful_story_when_available() {
        let mut generic = story("codex changed secret_repo", 78);
        generic.deck = "2 changed files.".to_string();
        let mut meaningful = story("browser route keeps feed identity stable", 86);
        meaningful.family = StoryFamily::Turn;
        meaningful.key.family = StoryFamily::Turn;
        meaningful.deck =
            "remote viewers can identify the account behind each public headline.".to_string();

        let summaries = summarize_feed(
            "local:workstation",
            &[generic, meaningful],
            &SummaryConfig::p2p_default(),
        )
        .expect("summary compiles");

        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].headline,
            "browser route keeps feed identity stable"
        );
        assert!(summaries[0].deck.contains("public headline"));
    }

    #[test]
    fn active_project_ci_outcome_survives_summary_gate() {
        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let mut story = public_project_story("burn_p2p", 76);
        story.family = StoryFamily::Turn;
        story.key.family = StoryFamily::Turn;
        story.headline = "burn_p2p release readiness turns green".to_string();
        story.deck = "only the remaining release lanes continue while the paired burn_dragon ci rerun stays active.".to_string();

        let summaries = summarize_feed("github:35904762:workstation", &[story], &config)
            .expect("summary compiles");

        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].headline,
            "burn_p2p release readiness turns green"
        );
        assert!(summaries[0].chips.iter().any(|chip| chip == "burn_p2p"));
    }

    #[test]
    fn deterministic_rollup_rewrites_agent_feed_scaffold_into_public_impact() {
        let mut story = story(
            "implemented the remaining slice around real Codex streams, settled summarization",
            90,
        );
        story.family = StoryFamily::Turn;
        story.key.family = StoryFamily::Turn;
        story.deck = "Implemented the remaining slice around real Codex streams, settled summarization, and p2p capsule tests.".to_string();

        let summaries = summarize_feed(
            "github:35904762:workstation",
            &[story],
            &SummaryConfig::p2p_default(),
        )
        .expect("summary compiles");

        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].headline,
            "feed turns live agent work into safer public story capsules"
        );
        assert!(summaries[0].deck.contains("settled, redacted headlines"));
    }

    #[test]
    fn generic_low_context_summaries_do_not_publish() {
        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let mut incident = story("codex command failed", 84);
        incident.family = StoryFamily::Incident;
        incident.headline = "codex shell command failed".to_string();
        incident.deck = "shell command failed.".to_string();
        incident.score = 84;
        incident.severity = Severity::Warning;

        let summaries =
            summarize_feed("local:workstation", &[incident], &config).expect("summary compiles");

        assert!(summaries.is_empty());
    }

    #[test]
    fn severe_generic_low_context_summaries_still_do_not_publish() {
        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let mut incident = story("codex command failed", 95);
        incident.family = StoryFamily::Incident;
        incident.headline = "codex shell command failed".to_string();
        incident.deck = "shell command failed.".to_string();
        incident.score = 95;
        incident.severity = Severity::Critical;

        let summaries =
            summarize_feed("local:workstation", &[incident], &config).expect("summary compiles");

        assert!(summaries.is_empty());
    }

    #[test]
    fn deterministic_rollup_does_not_emit_omission_copy() {
        let stories = (0..3)
            .map(|index| story(&format!("codex changed secret_repo {index}"), 78))
            .collect::<Vec<_>>();
        let summaries =
            summarize_feed("local:workstation", &stories, &SummaryConfig::p2p_default())
                .expect("summary compiles");

        let display = serde_json::to_string(&summaries)
            .expect("summaries serialize")
            .to_ascii_lowercase();
        assert!(!display.contains("raw detail omitted"));
        assert!(!display.contains("raw prompts"));
        assert!(!display.contains("command output"));
        assert!(!display.contains("repo names omitted"));
    }

    #[test]
    fn per_story_publish_skips_duplicate_headlines() {
        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let stories = vec![
            story("codex changed secret_repo first pass", 78),
            story("codex changed secret_repo second pass", 79),
        ];

        let summaries =
            summarize_feed("local:workstation", &stories, &config).expect("summary compiles");

        assert!(summaries.is_empty());
    }

    #[test]
    fn meaningful_headline_change_publishes_again() {
        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let first = story("codex changed secret_repo", 78);
        let mut second = story("codex verified tests", 79);
        second.family = StoryFamily::Test;
        second.headline = "codex verified tests".to_string();
        second.deck = "tests passed after the feed capture update. raw detail omitted.".to_string();

        let summaries = summarize_feed("local:workstation", &[first, second], &config)
            .expect("summary compiles");

        assert_eq!(summaries.len(), 1);
        assert!(
            summaries[0].metadata.max_headline_similarity < config.publish.max_headline_similarity
        );
    }

    #[test]
    fn recent_published_headline_suppresses_duplicate() {
        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let existing = RecentSummary {
            headline: "codex changed 1 files".to_string(),
            deck: "1 changed files. raw detail omitted.".to_string(),
            story_family: StoryFamily::FileChange,
            score: 78,
        };
        let stories = vec![story("codex changed secret_repo again", 78)];

        let summaries = summarize_feed_with_recent(
            "local:workstation",
            &stories,
            &config,
            std::slice::from_ref(&existing),
        )
        .expect("summary compiles");

        assert!(summaries.is_empty());
        assert_eq!(
            headline_similarity("codex changed 1 files", &existing.headline),
            100
        );
    }

    #[test]
    fn exact_duplicate_high_severity_summary_is_still_suppressed() {
        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let existing = RecentSummary {
            headline: "codex found failing tests".to_string(),
            deck: "tests are red. test command failed.".to_string(),
            story_family: StoryFamily::Test,
            score: 90,
        };
        let mut failed = verified_story(90);
        failed.headline = existing.headline.clone();
        failed.deck = existing.deck.clone();

        let summaries = summarize_feed_with_recent(
            "local:workstation",
            &[failed],
            &config,
            std::slice::from_ref(&existing),
        )
        .expect("summary compiles");

        assert!(summaries.is_empty());
    }

    #[test]
    fn guardrails_mask_pii_and_reject_credentials() {
        let guardrails = SummaryGuardrails::strict_p2p();
        let (cleaned, violations) = guardrails
            .clean_text("mail alice@example.com about project")
            .expect("email masks");
        assert!(cleaned.contains("[redacted]"));
        assert_eq!(violations[0].name, "email");

        let rejected = guardrails.clean_text("token sk-live_secret");
        assert!(matches!(rejected, Err(SummaryError::GuardrailRejected(_))));
    }

    #[test]
    fn guardrail_rejected_story_does_not_block_feed_publish() {
        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;

        let good = verified_story(90);
        let mut bad = story("codex found sk-liveSecret", 90);
        bad.headline = "codex found sk-liveSecret".to_string();
        bad.deck = "token sk-liveSecret appeared in output.".to_string();
        let summaries =
            summarize_feed("local:workstation", &[bad, good], &config).expect("summarizes");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].headline, "verified tests");
        let display = serde_json::to_string(&summaries).expect("summaries serialize");
        assert!(!display.contains("sk-liveSecret"));
    }

    #[test]
    fn rejected_rollup_falls_back_to_safe_story_summaries() {
        struct RejectingRollupProcessor;
        impl SummaryProcessor for RejectingRollupProcessor {
            fn name(&self) -> &str {
                "rejecting-rollup"
            }

            fn summarize(
                &self,
                request: &SummaryRequest,
            ) -> Result<ProcessorSummary, SummaryError> {
                if request.stories.len() > 1 {
                    return Ok(ProcessorSummary {
                        headline: "codex found sk-liveSecret".to_string(),
                        deck: "token sk-liveSecret appeared in output.".to_string(),
                        lower_third: Some("processor · redacted".to_string()),
                        chips: vec!["processor".to_string()],
                        publish: None,
                        publish_reason: None,
                        memory_digest: None,
                        semantic_fingerprint: None,
                    });
                }
                let story = &request.stories[0];
                Ok(ProcessorSummary {
                    headline: story.headline.clone(),
                    deck: story.deck.clone(),
                    lower_third: Some("processor · redacted".to_string()),
                    chips: story.chips.clone(),
                    publish: None,
                    publish_reason: None,
                    memory_digest: None,
                    semantic_fingerprint: None,
                })
            }
        }

        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::FeedRollup;

        let good = verified_story(90);
        let mut bad = story("codex found sk-liveSecret", 90);
        bad.headline = "codex found sk-liveSecret".to_string();
        bad.deck = "token sk-liveSecret appeared in output.".to_string();
        let summaries = summarize_feed_with_processor(
            "local:workstation",
            &[bad, good],
            &config,
            &RejectingRollupProcessor,
        )
        .expect("summarizes");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].headline, "verified tests");
        let display = serde_json::to_string(&summaries).expect("summaries serialize");
        assert!(!display.contains("sk-liveSecret"));
    }

    #[test]
    fn codex_and_claude_processor_configs_are_available() {
        assert_eq!(SummaryProcessorConfig::CodexExec.name(), "codex-exec");
        assert_eq!(
            SummaryProcessorConfig::CodexSessionMemory {
                store_path: "/tmp/agent-feed-memory.json".to_string(),
                key: "feed:test".to_string(),
                command: "codex".to_string(),
            }
            .name(),
            "codex-memory"
        );
        assert_eq!(SummaryProcessorConfig::ClaudeCodeExec.name(), "claude-code");
        let codex = SummaryProcessorConfig::codex_command();
        let claude = SummaryProcessorConfig::claude_command();
        let SummaryProcessorConfig::Process { args, .. } = codex else {
            panic!("expected codex process config");
        };
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "--json".to_string(),
                "--model".to_string(),
                DEFAULT_CODEX_SUMMARY_MODEL.to_string(),
            ]
        );
        assert!(matches!(claude, SummaryProcessorConfig::Process { .. }));
    }

    #[test]
    fn codex_jsonl_processor_output_extracts_session_and_summary() {
        let output = r#"{"type":"session_meta","payload":{"id":"019dbd66-7008-7122-8858-a94e3a7ad2f6"}}
{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"{\"headline\":\"codex resolved publish gating\",\"deck\":\"new events publish after the memory gate accepts them.\",\"chips\":[\"codex\",\"memory\"],\"memory_digest\":\"the feed now tracks post-start publish gating\",\"semantic_fingerprint\":\"publish:gating\"}"}}"#;

        let parsed = parse_processor_output_with_meta(output).expect("codex jsonl parses");

        assert_eq!(
            parsed.codex_session_id.as_deref(),
            Some("019dbd66-7008-7122-8858-a94e3a7ad2f6")
        );
        assert_eq!(parsed.summary.headline, "codex resolved publish gating");
        assert_eq!(
            parsed.summary.semantic_fingerprint.as_deref(),
            Some("publish:gating")
        );
    }

    #[test]
    fn codex_exec_json_processor_output_extracts_item_completed_summary() {
        let output = r#"{"type":"thread.started","thread_id":"019dc642-bc8e-7091-9f3a-348b14357257"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"{\"headline\":\"codex summarized active feed work\",\"deck\":\"capture fixes now produce settled stories from real transcripts.\",\"chips\":[\"codex\",\"capture\"],\"memory_digest\":\"capture summaries are flowing\",\"semantic_fingerprint\":\"capture:stories\"}"}}
{"type":"turn.completed"}"#;

        let parsed = parse_processor_output_with_meta(output).expect("codex exec json parses");

        assert_eq!(
            parsed.codex_session_id.as_deref(),
            Some("019dc642-bc8e-7091-9f3a-348b14357257")
        );
        assert_eq!(parsed.summary.headline, "codex summarized active feed work");
        assert_eq!(
            parsed.summary.semantic_fingerprint.as_deref(),
            Some("capture:stories")
        );
    }

    #[test]
    #[cfg(unix)]
    fn codex_memory_processor_resumes_stored_session() {
        use std::os::unix::fs::PermissionsExt;

        let root = env::temp_dir().join(format!(
            "agent-feed-codex-memory-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp dir creates");
        let script = root.join("fake-codex");
        let args_log = root.join("args.log");
        let env_log = root.join("env.log");
        let store = root.join("memory.json");
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
printf 'capture_disabled=%s processor=%s pwd=%s\n' "$AGENT_FEED_CAPTURE_DISABLED" "$AGENT_FEED_INTERNAL_PROCESSOR" "$PWD" >> '{}'
cat >/dev/null
printf '%s\n' '{{"type":"session_meta","payload":{{"id":"summary-session-1"}}}}'
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"task_complete","last_agent_message":"{{\"headline\":\"codex remembered feed context\",\"deck\":\"new publish only happens after meaning changes.\",\"chips\":[\"codex\",\"memory\"],\"memory_digest\":\"context retained\",\"semantic_fingerprint\":\"memory:retained\"}}"}}}}'
"#,
                args_log.display(),
                env_log.display()
            ),
        )
        .expect("script writes");
        let mut permissions = fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("script executable");

        let processor =
            CodexSessionMemoryProcessor::new(&store, "feed:test", script.display().to_string());
        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let request = SummaryRequest::new(
            "local:workstation",
            FeedSummaryMode::PerStory,
            vec![verified_story(88)],
        );

        let first = summarize_request_with_processor(&request, &config, &processor)
            .expect("first summary compiles");
        let second = summarize_request_with_processor(&request, &config, &processor)
            .expect("second summary compiles");

        let args = fs::read_to_string(&args_log).expect("args log reads");
        assert!(
            args.lines()
                .next()
                .is_some_and(|line| line.contains("exec --json --model gpt-5.3-codex-spark"))
        );
        assert!(
            args.lines()
                .nth(1)
                .is_some_and(|line| line.contains("exec resume --json --model gpt-5.3-codex-spark"))
        );
        assert!(args.contains("summary-session-1"));
        assert_eq!(first.headline, "remembered feed context");
        assert_eq!(second.headline, "remembered feed context");
        assert!(
            fs::read_to_string(&store)
                .expect("store reads")
                .contains("summary-session-1")
        );
        let env = fs::read_to_string(env_log).expect("env log reads");
        assert!(env.contains("capture_disabled=1"));
        assert!(env.contains("processor=summary"));
        assert!(env.contains("codex-memory-work"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn external_processor_request_carries_summary_prompt_policy() {
        let mut config = SummaryConfig::p2p_default();
        config.prompt.style = "late-night signal room; cinematic but terse".to_string();
        config.prompt.max_prompt_chars = 900;
        let request = SummaryRequest::new(
            "local:workstation",
            FeedSummaryMode::FeedRollup,
            vec![story("codex changed secret_repo", 78)],
        );

        let redacted = request_for_external_processor(&request, &config).expect("request redacts");
        assert_eq!(
            redacted.prompt_style.as_deref(),
            Some("late-night signal room; cinematic but terse")
        );
        assert_eq!(redacted.max_prompt_chars, Some(900));

        let prompt = processor_prompt(&redacted);
        assert!(prompt.contains("late-night signal room"));
        assert!(prompt.len() <= 900);
        assert!(!prompt.contains("secret_repo"));
    }

    #[test]
    fn external_processor_request_carries_safe_project_tags() {
        let config = SummaryConfig::p2p_default();
        let request = SummaryRequest::new(
            "local:workstation",
            FeedSummaryMode::FeedRollup,
            vec![public_project_story("burn_p2p", 88)],
        );

        let redacted = request_for_external_processor(&request, &config).expect("request redacts");
        assert_eq!(redacted.stories[0].project.as_deref(), Some("burn_p2p"));

        let prompt = processor_prompt(&redacted);
        assert!(prompt.contains("project=burn_p2p"));
        assert!(prompt.contains("Keep provided project labels in chips"));
    }

    #[test]
    fn external_processor_request_redacts_context_and_recent_history() {
        let config = SummaryConfig::p2p_default();
        let mut request = SummaryRequest::new(
            "local:workstation",
            FeedSummaryMode::PerStory,
            vec![story(
                "codex ran cargo test in secret_repo from /home/mosure/private",
                82,
            )],
        );
        request.branch = Some("feature/customer-secret".to_string());
        request.session_hint = Some("session-with-private-context".to_string());
        request.recent_summaries = vec![RecentSummary {
            headline: "codex changed secret_repo".to_string(),
            deck: "git push mentioned alice@example.com".to_string(),
            story_family: StoryFamily::FileChange,
            score: 78,
        }];

        let redacted = request_for_external_processor(&request, &config).expect("request redacts");
        assert_eq!(redacted.branch.as_deref(), Some("[redacted]"));
        assert_eq!(redacted.session_hint.as_deref(), Some("[redacted]"));
        assert_eq!(redacted.stories[0].project, None);
        assert_eq!(redacted.stories[0].key.project_hash, None);
        assert!(!redacted.stories[0].headline.contains("secret_repo"));
        assert!(!redacted.stories[0].headline.contains("/home/mosure"));
        assert!(!redacted.stories[0].headline.contains("cargo test"));
        assert!(
            !redacted.recent_summaries[0]
                .headline
                .contains("secret_repo")
        );
        assert!(!redacted.recent_summaries[0].deck.contains("git push"));
        assert!(
            !redacted.recent_summaries[0]
                .deck
                .contains("alice@example.com")
        );

        let prompt = processor_prompt(&redacted);
        assert!(!prompt.contains("secret_repo"));
        assert!(!prompt.contains("/home/mosure"));
        assert!(!prompt.contains("cargo test"));
        assert!(!prompt.contains("alice@example.com"));
    }

    #[test]
    fn deterministic_summary_preserves_safe_project_tags() {
        let config = SummaryConfig::p2p_default();
        let stories = vec![public_project_story("burn_dragon", 88)];

        let summaries =
            summarize_feed("github:35904762:workstation", &stories, &config).expect("summary");

        assert_eq!(summaries[0].chips[0], "burn_dragon");
        assert!(summaries[0].chips.iter().any(|chip| chip == "codex"));
        assert!(summaries[0].lower_third.starts_with("burn_dragon ·"));
        assert!(!summaries[0].lower_third.contains("[project]"));
        assert!(!summaries[0].chips.iter().any(|chip| chip == "[project]"));
        assert!(!summaries[0].headline.contains("burn_dragon"));
        assert!(!summaries[0].deck.contains("burn_dragon"));
    }

    #[test]
    fn project_placeholder_is_removed_from_public_copy() {
        let config = SummaryConfig::p2p_default();
        let mut story = public_project_story("burn_dragon", 88);
        story.headline = "[project] publish verification advances".to_string();
        story.deck = "[project] native smoke is blocked by runner disk exhaustion.".to_string();
        let summaries =
            summarize_feed("github:35904762:workstation", &[story], &config).expect("summary");

        let display = format!(
            "{} {} {}",
            summaries[0].headline, summaries[0].deck, summaries[0].lower_third
        );
        assert!(!display.contains("[project]"));
        assert!(summaries[0].lower_third.starts_with("burn_dragon ·"));
    }

    struct EndpointLikeProcessor;

    impl SummaryProcessor for EndpointLikeProcessor {
        fn name(&self) -> &str {
            "endpoint-like"
        }

        fn summarize(&self, request: &SummaryRequest) -> Result<ProcessorSummary, SummaryError> {
            assert!(!request.stories[0].headline.contains("secret_repo"));
            assert!(request.stories[0].project.is_none());
            Ok(ProcessorSummary {
                headline: "alice@example.com shipped secret_repo".to_string(),
                deck: "token was never shown".to_string(),
                lower_third: Some("endpoint · redacted".to_string()),
                chips: vec!["endpoint".to_string(), "alice@example.com".to_string()],
                publish: None,
                publish_reason: None,
                memory_digest: None,
                semantic_fingerprint: None,
            })
        }
    }

    #[test]
    fn custom_processor_still_runs_through_guardrails() {
        let stories = vec![story("codex changed secret_repo", 78)];
        let summaries = summarize_feed_with_processor(
            "local:workstation",
            &stories,
            &SummaryConfig::p2p_default(),
            &EndpointLikeProcessor,
        )
        .expect("custom processor summary compiles");

        assert!(!summaries[0].headline.contains("alice@example.com"));
        assert!(!summaries[0].headline.contains("secret_repo"));
        assert!(!summaries[0].chips.iter().any(|chip| chip.contains('@')));
    }

    struct UglyPublicCopyProcessor;

    impl SummaryProcessor for UglyPublicCopyProcessor {
        fn name(&self) -> &str {
            "ugly-public-copy"
        }

        fn summarize(&self, _request: &SummaryRequest) -> Result<ProcessorSummary, SummaryError> {
            Ok(ProcessorSummary {
                headline: "Codex advances production scaffold; test gate stays red".to_string(),
                deck: "Production scaffold, m0 signal path, real stream handling, summarization, capsule tests, error logging, and response-body delivery advanced across the feed work. Tests still report failures despite one passing verification s. Command lifecycle captured without command output.".to_string(),
                lower_third: Some("@mosure / workstation".to_string()),
                chips: vec![
                    "production scaffold".to_string(),
                    "m0 signal path".to_string(),
                    "tests red".to_string(),
                    "command lifecycle captured".to_string(),
                ],
                publish: None,
                publish_reason: None,
                memory_digest: None,
                semantic_fingerprint: None,
            })
        }
    }

    #[test]
    fn processor_public_copy_is_polished_before_publish() {
        let summaries = summarize_feed_with_processor(
            "github:35904762:workstation",
            &[verified_story(92)],
            &SummaryConfig::p2p_default(),
            &UglyPublicCopyProcessor,
        )
        .expect("ugly processor output is polished");

        assert_eq!(
            summaries[0].headline,
            "moves feed implementation; tests are still failing"
        );
        let display = format!(
            "{} {} {}",
            summaries[0].headline,
            summaries[0].deck,
            summaries[0].chips.join(" ")
        )
        .to_ascii_lowercase();
        assert!(!display.contains("production scaffold"));
        assert!(!display.contains("m0"));
        assert!(!display.contains("test gate"));
        assert!(!display.contains("test line"));
        assert!(!display.contains("advanced"));
        assert!(!display.contains("advances"));
        assert!(!display.contains("verification s"));
        assert!(!display.contains("command lifecycle captured"));
        assert!(!display.contains("without command output"));
        assert!(summaries[0].deck.ends_with('.'));
    }

    struct LowImpactPublicCopyProcessor {
        headline: &'static str,
        deck: &'static str,
    }

    impl SummaryProcessor for LowImpactPublicCopyProcessor {
        fn name(&self) -> &str {
            "low-impact-public-copy"
        }

        fn summarize(&self, _request: &SummaryRequest) -> Result<ProcessorSummary, SummaryError> {
            Ok(ProcessorSummary {
                headline: self.headline.to_string(),
                deck: self.deck.to_string(),
                lower_third: Some("@mosure / workstation".to_string()),
                chips: vec![
                    "codex".to_string(),
                    "ci status".to_string(),
                    "run state".to_string(),
                ],
                publish: None,
                publish_reason: None,
                memory_digest: None,
                semantic_fingerprint: None,
            })
        }
    }

    #[test]
    fn public_summary_rejects_agent_mechanics_without_impact() {
        let cases = [
            (
                "codex checks ci status, settles run state",
                "sixteen command events converged on ci status and left the run state settled.",
            ),
            (
                "codex changes two files, shifts feed to edits",
                "two files changed after the prior ci status summary.",
            ),
            (
                "codex verifies tests, confirms pass state",
                "tests passed after the two-file change.",
            ),
        ];

        for (headline, deck) in cases {
            let processor = LowImpactPublicCopyProcessor { headline, deck };
            let summaries = summarize_feed_with_processor(
                "github:35904762:workstation",
                &[verified_story(84)],
                &SummaryConfig::p2p_default(),
                &processor,
            )
            .expect("low-impact processor output is handled");

            assert!(
                summaries.is_empty(),
                "low-impact mechanical story should not publish: {headline}"
            );
        }
    }

    #[test]
    fn external_summary_prompt_demands_newsworthy_outcomes() {
        let request = SummaryRequest::new(
            "github:35904762:workstation",
            FeedSummaryMode::FeedRollup,
            vec![verified_story(84)],
        );
        let prompt = processor_prompt(&request);

        assert!(prompt.contains("compact news"));
        assert!(prompt.contains("why it matters"));
        assert!(prompt.contains("Do not start the headline with Codex"));
        assert!(prompt.contains("CI polling"));
        assert!(prompt.contains("file counts"));
    }

    struct LeakyPromptProcessor;

    impl SummaryProcessor for LeakyPromptProcessor {
        fn name(&self) -> &str {
            "leaky-prompt"
        }

        fn summarize(&self, _request: &SummaryRequest) -> Result<ProcessorSummary, SummaryError> {
            Ok(ProcessorSummary {
                headline: "feed capture update reaches subscribers. raw detail omitted.".to_string(),
                deck: "browser subscribers receive safer updates. raw detail omitted. Do not include raw prompts.".to_string(),
                lower_third: Some(
                    "raw prompts, command output, diffs, paths, and repo names omitted."
                        .to_string(),
                ),
                chips: vec!["raw detail omitted".to_string(), "codex".to_string()],
                publish: None,
                publish_reason: None,
                memory_digest: None,
                semantic_fingerprint: None,
            })
        }
    }

    #[test]
    fn processor_prompt_leakage_is_removed_from_summary_output() {
        let stories = vec![story("codex changed secret_repo", 78)];
        let summaries = summarize_feed_with_processor(
            "local:workstation",
            &stories,
            &SummaryConfig::p2p_default(),
            &LeakyPromptProcessor,
        )
        .expect("leaky processor output is sanitized");

        let summary = &summaries[0];
        let display = format!(
            "{} {} {} {}",
            summary.headline,
            summary.deck,
            summary.lower_third,
            summary.chips.join(" ")
        )
        .to_ascii_lowercase();
        assert!(!summary.headline.is_empty());
        assert!(!display.contains("raw detail omitted"));
        assert!(!display.contains("raw prompts"));
        assert!(!display.contains("command output"));
        assert!(!display.contains("do not include"));
        assert!(
            summary
                .metadata
                .violations
                .iter()
                .any(|violation| violation.name == PROMPT_LEAKAGE_VIOLATION)
        );
    }

    struct DecliningProcessor;

    impl SummaryProcessor for DecliningProcessor {
        fn name(&self) -> &str {
            "declining"
        }

        fn summarize(&self, request: &SummaryRequest) -> Result<ProcessorSummary, SummaryError> {
            assert_eq!(request.recent_summaries.len(), 1);
            Ok(ProcessorSummary {
                headline: "codex changed 1 files".to_string(),
                deck: "same update as the recent published summary.".to_string(),
                lower_third: Some("processor · redacted".to_string()),
                chips: vec!["processor".to_string(), "redacted".to_string()],
                publish: Some(false),
                publish_reason: Some("headline did not meaningfully change".to_string()),
                memory_digest: Some(
                    "codex changed one file earlier; no new public signal".to_string(),
                ),
                semantic_fingerprint: Some("changed:file".to_string()),
            })
        }
    }

    #[test]
    fn local_agent_processor_can_decline_publish() {
        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let request = SummaryRequest {
            feed_id: "local:workstation".to_string(),
            mode: FeedSummaryMode::PerStory,
            stories: vec![story("codex changed secret_repo", 78)],
            recent_summaries: vec![RecentSummary {
                headline: "codex changed 1 files".to_string(),
                deck: "1 changed files. raw detail omitted.".to_string(),
                story_family: StoryFamily::FileChange,
                score: 78,
            }],
            prompt_style: None,
            max_prompt_chars: None,
            branch: None,
            session_hint: None,
        };
        let mut summary = summarize_request_with_processor(&request, &config, &DecliningProcessor)
            .expect("declining processor returns a display-safe candidate");

        assert!(!apply_publish_decision(
            &mut summary,
            &request,
            &config.publish
        ));
        assert_eq!(
            summary.metadata.publish_action,
            PublishAction::SkipProcessor
        );
    }

    struct StaticImageProcessor {
        image: Option<HeadlineImage>,
    }

    impl ImageProcessor for StaticImageProcessor {
        fn name(&self) -> &str {
            "static-image"
        }

        fn summarize_image(
            &self,
            request: &ImageRequest,
        ) -> Result<Option<HeadlineImage>, SummaryError> {
            assert!(!request.headline.contains("secret_repo"));
            assert!(!request.deck.contains("/home/"));
            Ok(self.image.clone())
        }
    }

    #[test]
    fn headline_images_are_disabled_by_default() {
        let stories = vec![verified_story(82)];
        let summaries =
            summarize_feed("local:workstation", &stories, &SummaryConfig::p2p_default())
                .expect("summary compiles");

        assert!(summaries[0].image.is_none());
        assert!(!summaries[0].metadata.image_enabled);
        assert_eq!(summaries[0].metadata.image_processor, "disabled");
    }

    #[test]
    fn image_processor_may_decline_headline_image() {
        let mut config = SummaryConfig::p2p_default();
        config.image.enabled = true;
        config.image.decision = ImageDecisionMode::AlwaysAsk;
        let stories = vec![verified_story(82)];
        let processor = StaticImageProcessor { image: None };

        let summaries = summarize_feed_with_processors(
            "local:workstation",
            &stories,
            &config,
            None,
            Some(&processor),
        )
        .expect("summary compiles");

        assert!(summaries[0].image.is_none());
        assert!(summaries[0].metadata.image_enabled);
    }

    #[test]
    fn headline_image_attaches_when_enabled_and_safe() {
        let mut config = SummaryConfig::p2p_default();
        config.image.enabled = true;
        config.image.decision = ImageDecisionMode::AlwaysAsk;
        let stories = vec![verified_story(82)];
        let processor = StaticImageProcessor {
            image: Some(HeadlineImage::new(
                "/assets/headlines/settled-story.webp",
                "abstract signal lines around a completed agent task",
                "static-image",
            )),
        };

        let summaries = summarize_feed_with_processors(
            "local:workstation",
            &stories,
            &config,
            None,
            Some(&processor),
        )
        .expect("summary compiles");

        let image = summaries[0].image.as_ref().expect("image attached");
        assert_eq!(image.uri, "/assets/headlines/settled-story.webp");
        assert!(image.alt.contains("completed agent task"));
    }

    #[test]
    fn headline_image_uri_and_alt_pass_guardrails() {
        let mut config = SummaryConfig::p2p_default();
        config.image.enabled = true;
        config.image.decision = ImageDecisionMode::AlwaysAsk;
        let stories = vec![verified_story(82)];
        let processor = StaticImageProcessor {
            image: Some(HeadlineImage::new(
                "https://example.com/secret.png",
                "contains token=abc",
                "static-image",
            )),
        };

        let error = summarize_feed_with_processors(
            "local:workstation",
            &stories,
            &config,
            None,
            Some(&processor),
        )
        .expect_err("unsafe image is rejected");

        assert!(matches!(error, SummaryError::Processor(_)));
    }

    #[test]
    fn http_endpoint_receives_redacted_story_facts() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::thread;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local endpoint");
        let address = listener.local_addr().expect("local endpoint address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("endpoint accepts request");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout configures");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let mut content_len = None;
            loop {
                let read = stream.read(&mut buffer).expect("request reads");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4);
                if content_len.is_none()
                    && let Some(header_end) = header_end
                {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    content_len = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        if name.eq_ignore_ascii_case("content-length") {
                            value.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    });
                }
                if let (Some(header_end), Some(content_len)) = (header_end, content_len)
                    && request.len() >= header_end + content_len
                {
                    break;
                }
            }
            let request_text = String::from_utf8(request).expect("request is utf-8");
            assert!(request_text.starts_with("POST /summary "));
            assert!(!request_text.contains("secret_repo"));
            assert!(!request_text.contains("alice@example.com"));
            let body = r#"{"headline":"alice@example.com shipped secret_repo","deck":"safe external summary","chips":["endpoint","alice@example.com"]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("response writes");
            request_text
        });

        let mut config = SummaryConfig::p2p_default();
        config.processor = SummaryProcessorConfig::HttpEndpoint {
            url: format!("http://{address}/summary"),
            auth_header_env: None,
        };
        let stories = vec![story("alice@example.com changed secret_repo", 78)];
        let summaries =
            summarize_feed("local:workstation", &stories, &config).expect("endpoint summarizes");
        let request_text = handle.join().expect("endpoint thread joins");

        assert!(request_text.contains("POST /summary"));
        assert_eq!(summaries[0].metadata.processor, "http-endpoint");
        assert!(!summaries[0].headline.contains("alice@example.com"));
        assert!(!summaries[0].headline.contains("secret_repo"));
        assert!(!summaries[0].chips.iter().any(|chip| chip.contains('@')));
    }
}
