use agent_feed_core::{AgentEvent, PrivacyClass};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrivacyConfig {
    pub mode: PrivacyMode,
    pub hash_paths: bool,
    pub mask_home: bool,
    pub show_prompts: bool,
    pub show_command_output: bool,
    pub show_diffs: bool,
    pub redact_patterns: Vec<String>,
    pub redact_paths: Vec<String>,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            mode: PrivacyMode::Aggressive,
            hash_paths: true,
            mask_home: true,
            show_prompts: false,
            show_command_output: false,
            show_diffs: false,
            redact_patterns: vec![
                "sk-[A-Za-z0-9_-]+".to_string(),
                "ghp_[A-Za-z0-9_]+".to_string(),
                "AKIA[0-9A-Z]{16}".to_string(),
            ],
            redact_paths: vec![
                ".env".to_string(),
                ".env.".to_string(),
                "secrets/".to_string(),
                ".pem".to_string(),
                ".key".to_string(),
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    #[default]
    Aggressive,
    Balanced,
    Debug,
}

#[derive(Clone, Debug)]
pub struct Redactor {
    config: PrivacyConfig,
    home: Option<String>,
    patterns: Vec<Regex>,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new(PrivacyConfig::default())
    }
}

impl Redactor {
    #[must_use]
    pub fn new(config: PrivacyConfig) -> Self {
        let patterns = config
            .redact_patterns
            .iter()
            .filter_map(|pattern| Regex::new(pattern).ok())
            .collect();
        let home = env::var("HOME").ok().filter(|home| !home.is_empty());
        Self {
            config,
            home,
            patterns,
        }
    }

    #[must_use]
    pub fn redact_event(&self, mut event: AgentEvent) -> AgentEvent {
        event.title = self.redact_text(&event.title);
        event.summary = event.summary.map(|value| self.redact_text(&value));
        event.command = event.command.map(|value| self.redact_text(&value));
        event.uri = event.uri.map(|value| self.redact_uri(&value));
        event.cwd = event.cwd.map(|value| self.redact_path(&value));
        event.files = event
            .files
            .into_iter()
            .map(|value| self.redact_path(&value))
            .collect();
        event.privacy = PrivacyClass::Redacted;
        event
    }

    #[must_use]
    pub fn redact_text(&self, input: &str) -> String {
        let mut output = input.to_string();
        for pattern in &self.patterns {
            output = pattern.replace_all(&output, "[redacted]").into_owned();
        }

        if self.config.mask_home
            && let Some(home) = &self.home
        {
            output = output.replace(home, "~");
        }

        output
    }

    #[must_use]
    pub fn redact_uri(&self, input: &str) -> String {
        let without_query = input.split_once('?').map_or(input, |(head, _)| head);
        self.redact_text(without_query)
    }

    #[must_use]
    pub fn redact_path(&self, input: &str) -> String {
        if self.is_sensitive_path(input) {
            return "[sensitive-path]".to_string();
        }

        if self.config.mask_home
            && let Some(home) = &self.home
            && let Some(rest) = input.strip_prefix(home)
        {
            let rest = rest.trim_start_matches('/');
            if self.config.hash_paths {
                return format!("~/#{}", short_hash(rest));
            }
            return format!("~/{rest}");
        }

        if self.config.hash_paths && input.starts_with('/') {
            return format!("/#{}", short_hash(input));
        }

        input.to_string()
    }

    fn is_sensitive_path(&self, input: &str) -> bool {
        let lowered = input.to_ascii_lowercase();
        self.config.redact_paths.iter().any(|pattern| {
            if pattern.ends_with('/') {
                lowered.contains(pattern)
            } else if pattern.starts_with('.') {
                lowered.ends_with(pattern) || lowered.contains(pattern)
            } else {
                lowered.contains(pattern)
            }
        })
    }
}

fn short_hash(input: &str) -> String {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    format!("{:08x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_core::{AgentEvent, EventKind, SourceKind};

    #[test]
    fn masks_secrets_and_sensitive_paths() {
        let redactor = Redactor::default();
        let mut event = AgentEvent::new(
            SourceKind::Generic,
            EventKind::CommandExec,
            "ran command with sk-abc123",
        );
        event.command = Some("echo ghp_secretvalue".to_string());
        event.files = vec![
            "/tmp/project/.env".to_string(),
            "/tmp/project/src/main.rs".to_string(),
        ];

        let event = redactor.redact_event(event);

        assert_eq!(event.title, "ran command with [redacted]");
        assert_eq!(event.command.as_deref(), Some("echo [redacted]"));
        assert_eq!(event.files[0], "[sensitive-path]");
        assert!(event.files[1].starts_with("/#"));
    }
}
