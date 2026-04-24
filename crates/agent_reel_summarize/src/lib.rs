use agent_reel_core::{HeadlineImage, PrivacyClass, Severity};
use agent_reel_story::{CompiledStory, StoryFamily};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

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
            max_feed_rollup_stories: 32,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardrailViolation {
    pub name: String,
    pub action: GuardrailAction,
}

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

impl SummaryProcessorConfig {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::CodexExec => "codex-exec",
            Self::ClaudeCodeExec => "claude-code",
            Self::Process { .. } => "process",
            Self::HttpEndpoint { .. } => "http-endpoint",
        }
    }

    #[must_use]
    pub fn codex_command() -> Self {
        Self::Process {
            command: "codex".to_string(),
            args: vec!["exec".to_string(), "--json".to_string()],
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
    pub image: ImageConfig,
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
            image: ImageConfig::default(),
            budget: SummaryBudget::default(),
            guardrails: SummaryGuardrails::strict_p2p(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SummaryRequest {
    pub feed_id: String,
    pub mode: FeedSummaryMode,
    pub stories: Vec<CompiledStory>,
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

fn run_process(command: &str, args: &[String], stdin_text: &str) -> Result<String, SummaryError> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

    let output = child
        .wait_with_output()
        .map_err(|err| SummaryError::Processor(format!("wait failed: {err}")))?;
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
        .read_to_string(&mut response)
        .map_err(|err| SummaryError::Processor(format!("http response read failed: {err}")))?;
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
    summarize_feed_inner(feed_id, stories, config, None, None)
}

pub fn summarize_feed_with_processor(
    feed_id: &str,
    stories: &[CompiledStory],
    config: &SummaryConfig,
    processor: &dyn SummaryProcessor,
) -> Result<Vec<FeedSummary>, SummaryError> {
    summarize_feed_inner(feed_id, stories, config, Some(processor), None)
}

pub fn summarize_feed_with_processors(
    feed_id: &str,
    stories: &[CompiledStory],
    config: &SummaryConfig,
    processor: Option<&dyn SummaryProcessor>,
    image_processor: Option<&dyn ImageProcessor>,
) -> Result<Vec<FeedSummary>, SummaryError> {
    summarize_feed_inner(feed_id, stories, config, processor, image_processor)
}

fn summarize_feed_inner(
    feed_id: &str,
    stories: &[CompiledStory],
    config: &SummaryConfig,
    processor: Option<&dyn SummaryProcessor>,
    image_processor: Option<&dyn ImageProcessor>,
) -> Result<Vec<FeedSummary>, SummaryError> {
    if stories.is_empty() {
        return Ok(Vec::new());
    }

    let batches: Vec<Vec<CompiledStory>> = match config.mode {
        FeedSummaryMode::PerStory => stories.iter().cloned().map(|story| vec![story]).collect(),
        FeedSummaryMode::FeedRollup => stories
            .chunks(config.budget.max_feed_rollup_stories.max(1))
            .map(<[CompiledStory]>::to_vec)
            .collect(),
    };

    let mut summaries = Vec::new();
    for batch in batches {
        let request = SummaryRequest::new(feed_id, config.mode, batch);
        summaries.push(summarize_request_inner(
            &request,
            config,
            processor,
            image_processor,
        )?);
    }
    Ok(summaries)
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
                    vec!["exec".to_string(), "--json".to_string()],
                )
                .summarize(&processor_request)?
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
    Ok(summary)
}

fn request_for_external_processor(
    request: &SummaryRequest,
    config: &SummaryConfig,
) -> Result<SummaryRequest, SummaryError> {
    let mut redacted = request.clone();
    redacted.branch = request.branch.as_ref().map(|_| "[redacted]".to_string());
    redacted.session_hint = request
        .session_hint
        .as_ref()
        .map(|_| "[redacted]".to_string());
    for story in &mut redacted.stories {
        story.key.project_hash = None;
        if !config.guardrails.allow_project_names {
            story.project = None;
        }
        story.headline = clean_and_clamp(
            &story.headline,
            config.budget.max_headline_chars,
            &config.guardrails,
        )?
        .0;
        story.deck = clean_and_clamp(
            &story.deck,
            config.budget.max_deck_chars,
            &config.guardrails,
        )?
        .0;
        story.lower_third = clean_and_clamp(
            &story.lower_third,
            config.budget.max_lower_third_chars,
            &config.guardrails,
        )?
        .0;
        story.chips = story
            .chips
            .iter()
            .take(config.budget.max_chips)
            .map(|chip| {
                clean_and_clamp(chip, config.budget.max_chip_chars, &config.guardrails)
                    .map(|(chip, _)| chip)
            })
            .collect::<Result<Vec<_>, _>>()?;
    }
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
                story.family,
                story.score,
            )),
            chips: story
                .chips
                .iter()
                .filter(|chip| !chip.contains('_') && !chip.contains('/'))
                .cloned()
                .collect(),
        };
    }

    let input_count = request.stories.len();
    let agents = request
        .stories
        .iter()
        .map(|story| story.agent.clone())
        .collect::<BTreeSet<_>>();
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

    let agent_label = if agents.len() == 1 {
        agents
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(|| "agents".to_string())
    } else {
        format!("{} agents", agents.len())
    };
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
    facts.push("raw prompts, command output, diffs, paths, and repo names omitted".to_string());

    ProcessorSummary {
        headline: format!("{agent_label} feed activity settled"),
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
    }
}

fn build_summary(
    request: &SummaryRequest,
    processor_output: ProcessorSummary,
    config: &SummaryConfig,
) -> Result<FeedSummary, SummaryError> {
    let budget = &config.budget;
    let guardrails = &config.guardrails;
    let (headline, mut violations) = clean_and_clamp(
        &processor_output.headline,
        budget.max_headline_chars,
        guardrails,
    )?;
    let (deck, deck_violations) =
        clean_and_clamp(&processor_output.deck, budget.max_deck_chars, guardrails)?;
    violations.extend(deck_violations);
    let (lower_third, lower_violations) = clean_and_clamp(
        processor_output
            .lower_third
            .as_deref()
            .unwrap_or("feed · redacted"),
        budget.max_lower_third_chars,
        guardrails,
    )?;
    violations.extend(lower_violations);

    let mut chips = Vec::new();
    for chip in processor_output.chips.into_iter().take(budget.max_chips) {
        let (chip, chip_violations) = clean_and_clamp(&chip, budget.max_chip_chars, guardrails)?;
        violations.extend(chip_violations);
        if !chip.is_empty() && !chips.iter().any(|existing| existing == &chip) {
            chips.push(chip);
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
    fit_capsule_budget(&mut summary, budget, guardrails)?;
    summary.metadata.output_chars =
        summary.headline.len() + summary.deck.len() + summary.lower_third.len();
    Ok(summary)
}

fn clean_and_clamp(
    value: &str,
    max_chars: usize,
    guardrails: &SummaryGuardrails,
) -> Result<(String, Vec<GuardrailViolation>), SummaryError> {
    let mut input = value.to_string();
    if !guardrails.allow_project_names {
        input = mask_project_like_terms(&input);
    }
    if !guardrails.allow_command_text {
        input = mask_command_like_terms(&input);
    }
    let (cleaned, violations) = guardrails.clean_text(&input)?;
    Ok((clamp_chars(&cleaned, max_chars), violations))
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
    let (deck, violations) = guardrails.clean_text(&summary.deck)?;
    summary.metadata.violations.extend(violations);
    summary.deck = deck;
    Ok(())
}

fn processor_prompt(request: &SummaryRequest) -> String {
    let stories = request
        .stories
        .iter()
        .map(|story| {
            format!(
                "- agent={} family={:?} score={} headline={} deck={}",
                story.agent, story.family, story.score, story.headline, story.deck
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Return one JSON object with headline, deck, lower_third, and chips. Use only the redacted story facts below. Do not include raw prompts, command output, diffs, absolute paths, repo names, emails, secrets, tokens, credentials, or personal data.\nfeed={}\nmode={:?}\n{}",
        request.feed_id, request.mode, stories
    )
}

fn image_processor_prompt(request: &ImageRequest) -> String {
    let base = format!(
        "Return one JSON object. Either return {{\"image\": null, \"reason\": \"...\"}} when no useful projection-safe image exists, or return {{\"image\": {{\"uri\": \"...\", \"alt\": \"...\", \"source\": \"generated\"}}}}. Generate or reference only display-safe imagery. Do not include raw prompts, readable code, command output, diffs, secrets, tokens, credentials, exact paths, repo names, emails, or personal data. Use this visual style: {}.\nheadline={}\ndeck={}\nlower_third={}\nchips={}\nfamily={:?}\nscore={}",
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

fn parse_processor_output(output: &str) -> Result<ProcessorSummary, SummaryError> {
    if let Ok(summary) = serde_json::from_str::<ProcessorSummary>(output) {
        return Ok(summary);
    }
    Ok(ProcessorSummary {
        headline: "feed activity settled".to_string(),
        deck: clamp_chars(output.trim(), 220),
        lower_third: Some("external processor · redacted".to_string()),
        chips: vec!["external".to_string(), "redacted".to_string()],
    })
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

fn strict_lower_third(agent: &str, family: StoryFamily, score: u8) -> String {
    format!("{agent} · {:?} · score {score} · redacted", family)
}

fn mask_project_like_terms(input: &str) -> String {
    input
        .split_whitespace()
        .map(|word| {
            if word.contains('_') || word.contains('/') || word.ends_with(".rs") {
                "[project]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    let mut output = input
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_reel_core::{EventKind, SourceKind};
    use agent_reel_story::compile_events;

    fn story(title: &str, score: u8) -> CompiledStory {
        let mut event =
            agent_reel_core::AgentEvent::new(SourceKind::Codex, EventKind::FileChanged, title);
        event.agent = "codex".to_string();
        event.project = Some("secret_repo".to_string());
        event.session_id = Some("session".to_string());
        event.turn_id = Some(format!("turn-{score}"));
        event.files = vec!["src/lib.rs".to_string()];
        event.summary = Some("1 changed files. raw diff omitted.".to_string());
        event.score_hint = Some(score);
        compile_events([event]).remove(0)
    }

    #[test]
    fn p2p_rollup_reduces_many_stories_to_one_bounded_summary() {
        let stories = (0..12)
            .map(|index| story(&format!("codex changed secret_repo {index}"), 78))
            .collect::<Vec<_>>();
        let summaries =
            summarize_feed("local:workstation", &stories, &SummaryConfig::p2p_default())
                .expect("summary compiles");

        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].metadata.output_chars <= SummaryBudget::default().max_capsule_chars);
        assert!(!summaries[0].headline.contains("secret_repo"));
        assert!(!summaries[0].deck.contains("secret_repo"));
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
    fn codex_and_claude_processor_configs_are_available() {
        assert_eq!(SummaryProcessorConfig::CodexExec.name(), "codex-exec");
        assert_eq!(SummaryProcessorConfig::ClaudeCodeExec.name(), "claude-code");
        let codex = SummaryProcessorConfig::codex_command();
        let claude = SummaryProcessorConfig::claude_command();
        assert!(matches!(codex, SummaryProcessorConfig::Process { .. }));
        assert!(matches!(claude, SummaryProcessorConfig::Process { .. }));
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
        let stories = vec![story("codex changed secret_repo", 82)];
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
        let stories = vec![story("codex changed secret_repo", 82)];
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
        let stories = vec![story("codex changed secret_repo", 82)];
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
        let stories = vec![story("codex changed secret_repo", 82)];
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
