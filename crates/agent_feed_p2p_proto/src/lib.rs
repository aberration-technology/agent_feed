use agent_feed_core::{HeadlineImage, PrivacyClass, Severity};
use agent_feed_story::{CompiledStory, StoryFamily};
use agent_feed_summarize::{
    FeedSummary, SummaryConfig, SummaryError, SummaryMetadata, summarize_feed,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

type HmacSha256 = Hmac<Sha256>;
const SIGNATURE_DOMAIN: &[u8] = b"agent-feed-signature-v1";

pub type FeedId = String;
pub type NetworkId = String;
pub type PrincipalRef = String;
pub type PeerIdString = String;
pub type AvatarRef = String;
pub type Capability = String;
pub type CapsuleId = String;
pub type StoryWindowRef = String;
pub type AgentKind = String;

pub const AGENT_FEED_PRODUCT: &str = "feed";
pub const AGENT_FEED_PROTOCOL_NAME: &str = "agent-feed";
pub const AGENT_FEED_PROTOCOL_VERSION: u16 = 1;
pub const AGENT_FEED_MODEL_VERSION: u16 = 3;
pub const AGENT_FEED_MIN_MODEL_VERSION: u16 = 3;
pub const AGENT_FEED_EDGE_PROTOCOL: &str = "agent-feed.edge/1";
pub const AGENT_FEED_RELEASE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    Summary(#[from] SummaryError),
    #[error("story rejected by p2p summary policy")]
    StoryRejected,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolCompatibility {
    pub product: String,
    pub release_version: String,
    pub protocol_version: u16,
    pub model_version: u16,
    pub min_model_version: u16,
}

impl ProtocolCompatibility {
    #[must_use]
    pub fn current() -> Self {
        Self {
            product: AGENT_FEED_PRODUCT.to_string(),
            release_version: AGENT_FEED_RELEASE_VERSION.to_string(),
            protocol_version: AGENT_FEED_PROTOCOL_VERSION,
            model_version: AGENT_FEED_MODEL_VERSION,
            min_model_version: AGENT_FEED_MIN_MODEL_VERSION,
        }
    }

    #[must_use]
    pub fn with_model_version(mut self, model_version: u16, min_model_version: u16) -> Self {
        self.model_version = model_version;
        self.min_model_version = min_model_version;
        self
    }

    #[must_use]
    pub fn with_protocol_version(mut self, protocol_version: u16) -> Self {
        self.protocol_version = protocol_version;
        self
    }

    #[must_use]
    pub fn is_compatible_with(&self, remote: &Self) -> bool {
        self.product == remote.product
            && self.protocol_version == remote.protocol_version
            && self.model_version >= remote.min_model_version
            && remote.model_version >= self.min_model_version
    }

    #[must_use]
    pub fn status_with(&self, remote: &Self) -> CompatibilityStatus {
        CompatibilityStatus {
            compatible: self.is_compatible_with(remote),
            local: self.clone(),
            remote: remote.clone(),
            message: compatibility_message(self, remote),
        }
    }
}

impl Default for ProtocolCompatibility {
    fn default() -> Self {
        Self::current()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompatibilityStatus {
    pub compatible: bool,
    pub local: ProtocolCompatibility,
    pub remote: ProtocolCompatibility,
    pub message: String,
}

impl CompatibilityStatus {
    #[must_use]
    pub fn current() -> Self {
        let current = ProtocolCompatibility::current();
        current.status_with(&current)
    }
}

fn compatibility_message(local: &ProtocolCompatibility, remote: &ProtocolCompatibility) -> String {
    if local.is_compatible_with(remote) {
        "compatible".to_string()
    } else if local.product != remote.product {
        "incompatible product".to_string()
    } else if local.protocol_version != remote.protocol_version {
        "protocol changed; update your peer to the latest version".to_string()
    } else if remote.min_model_version > local.model_version {
        "remote feed requires a newer data model; update your peer to the latest version"
            .to_string()
    } else if local.min_model_version > remote.model_version {
        "remote peer is using an older data model; ask the publisher to update".to_string()
    } else {
        "incompatible feed data model".to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub key_id: String,
    pub digest: String,
}

impl Signature {
    #[must_use]
    pub fn unsigned() -> Self {
        Self {
            key_id: "unsigned".to_string(),
            digest: String::new(),
        }
    }

    pub fn for_value<T: Serialize>(key_id: &str, value: &T) -> Result<Self, ProtoError> {
        Ok(Self {
            key_id: key_id.to_string(),
            digest: sha256_signature_digest(key_id, &serde_json::to_vec(value)?),
        })
    }

    pub fn for_value_hmac<T: Serialize>(
        key_id: &str,
        secret: &str,
        value: &T,
    ) -> Result<Self, ProtoError> {
        Ok(Self {
            key_id: key_id.to_string(),
            digest: hmac_signature_digest(key_id, secret, &serde_json::to_vec(value)?),
        })
    }

    pub fn verify_value<T: Serialize>(&self, value: &T) -> Result<bool, ProtoError> {
        Ok(Signature::for_value(&self.key_id, value)? == *self)
    }

    pub fn verify_value_hmac<T: Serialize>(
        &self,
        secret: &str,
        value: &T,
    ) -> Result<bool, ProtoError> {
        let expected = Signature::for_value_hmac(&self.key_id, secret, value)?;
        Ok(constant_time_eq(
            expected.digest.as_bytes(),
            self.digest.as_bytes(),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn for_value<T: Serialize>(value: &T) -> Result<Self, ProtoError> {
        Ok(Self(stable_digest(&serde_json::to_vec(value)?)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed<T> {
    pub value: T,
    pub signature: Signature,
}

impl<T: Serialize> Signed<T> {
    pub fn sign(value: T, key_id: &str) -> Result<Self, ProtoError> {
        let signature = Signature::for_value(key_id, &value)?;
        Ok(Self { value, signature })
    }

    pub fn verify(&self) -> Result<bool, ProtoError> {
        self.signature.verify_value(&self.value)
    }

    pub fn sign_with_secret(value: T, key_id: &str, secret: &str) -> Result<Self, ProtoError> {
        let signature = Signature::for_value_hmac(key_id, secret, &value)?;
        Ok(Self { value, signature })
    }

    pub fn verify_with_secret(&self, secret: &str) -> Result<bool, ProtoError> {
        self.signature.verify_value_hmac(secret, &self.value)
    }
}

impl Signed<StoryCapsule> {
    pub fn sign_capsule(capsule: StoryCapsule, key_id: &str) -> Result<Self, ProtoError> {
        let value = capsule.sign(key_id)?;
        Ok(Self {
            signature: value.signature.clone(),
            value,
        })
    }

    pub fn verify_capsule(&self) -> Result<bool, ProtoError> {
        Ok(self.signature == self.value.signature
            && ProtocolCompatibility::current().is_compatible_with(&self.value.compatibility)
            && self.value.verify_signature()?)
    }

    pub fn sign_capsule_with_secret(
        capsule: StoryCapsule,
        key_id: &str,
        secret: &str,
    ) -> Result<Self, ProtoError> {
        let value = capsule.sign_with_secret(key_id, secret)?;
        Ok(Self {
            signature: value.signature.clone(),
            value,
        })
    }

    pub fn verify_capsule_with_secret(&self, secret: &str) -> Result<bool, ProtoError> {
        Ok(self.signature == self.value.signature
            && ProtocolCompatibility::current().is_compatible_with(&self.value.compatibility)
            && self.value.verify_signature_with_secret(secret)?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedVisibility {
    Private,
    GithubUser,
    GithubOrg,
    GithubTeam,
    GithubRepo,
    Public,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeedProfile {
    pub feed_id: FeedId,
    pub network_id: NetworkId,
    pub compatibility: ProtocolCompatibility,
    pub owner: PrincipalRef,
    pub peer_id: PeerIdString,
    pub label: String,
    pub display_name: String,
    pub avatar: Option<AvatarRef>,
    pub visibility: FeedVisibility,
    pub capabilities: Vec<Capability>,
    pub current_epoch: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub signature: Signature,
}

impl FeedProfile {
    #[must_use]
    pub fn new(
        feed_id: impl Into<String>,
        network_id: impl Into<String>,
        owner: impl Into<String>,
        peer_id: impl Into<String>,
        label: impl Into<String>,
        visibility: FeedVisibility,
    ) -> Self {
        let now = OffsetDateTime::now_utc();
        let label = label.into();
        let owner = owner.into();
        Self {
            feed_id: feed_id.into(),
            network_id: network_id.into(),
            compatibility: ProtocolCompatibility::current(),
            owner: owner.clone(),
            peer_id: peer_id.into(),
            label: label.clone(),
            display_name: format!("{owner} / {label}"),
            avatar: None,
            visibility,
            capabilities: vec!["story-capsule".to_string()],
            current_epoch: 0,
            created_at: now,
            updated_at: now,
            signature: Signature::unsigned(),
        }
    }

    pub fn sign(mut self, key_id: &str) -> Result<Self, ProtoError> {
        self.signature = Signature::unsigned();
        self.signature = Signature::for_value(key_id, &self)?;
        Ok(self)
    }

    pub fn verify_signature(&self) -> Result<bool, ProtoError> {
        let mut unsigned = self.clone();
        unsigned.signature = Signature::unsigned();
        self.signature.verify_value(&unsigned)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoryKind {
    Turn,
    Plan,
    Test,
    Permission,
    Command,
    FileChange,
    Mcp,
    Incident,
    Recap,
}

impl From<StoryFamily> for StoryKind {
    fn from(value: StoryFamily) -> Self {
        match value {
            StoryFamily::Turn => Self::Turn,
            StoryFamily::Plan => Self::Plan,
            StoryFamily::Test => Self::Test,
            StoryFamily::Permission => Self::Permission,
            StoryFamily::Command => Self::Command,
            StoryFamily::FileChange => Self::FileChange,
            StoryFamily::Mcp => Self::Mcp,
            StoryFamily::Incident => Self::Incident,
            StoryFamily::IdleRecap => Self::Recap,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvidenceDigest {
    pub event_id: String,
    pub digest: String,
}

impl EvidenceDigest {
    #[must_use]
    pub fn from_event_id(event_id: impl Into<String>) -> Self {
        let event_id = event_id.into();
        Self {
            digest: stable_digest(event_id.as_bytes()),
            event_id,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PublisherIdentity {
    pub github_user_id: Option<u64>,
    pub github_login: Option<String>,
    pub display_name: Option<String>,
    pub avatar: Option<AvatarRef>,
    pub verified: bool,
}

impl PublisherIdentity {
    #[must_use]
    pub fn github(
        github_user_id: u64,
        github_login: impl Into<String>,
        display_name: Option<String>,
        avatar: Option<AvatarRef>,
    ) -> Self {
        Self {
            github_user_id: Some(github_user_id),
            github_login: Some(github_login.into()),
            display_name,
            avatar,
            verified: true,
        }
    }

    #[must_use]
    pub fn display_label(&self) -> String {
        self.github_login
            .as_ref()
            .map(|login| format!("@{login}"))
            .or_else(|| self.display_name.clone())
            .unwrap_or_else(|| "verified peer".to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoryCapsule {
    pub capsule_id: CapsuleId,
    pub feed_id: FeedId,
    #[serde(default)]
    pub compatibility: ProtocolCompatibility,
    pub seq: u64,
    pub story_window: StoryWindowRef,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub author: PrincipalRef,
    pub source_agent_kinds: Vec<AgentKind>,
    pub headline: String,
    pub deck: String,
    pub lower_third: String,
    pub chips: Vec<String>,
    pub image: Option<HeadlineImage>,
    pub story_kind: StoryKind,
    pub severity: Severity,
    pub score: u8,
    pub privacy_class: PrivacyClass,
    pub evidence: Vec<EvidenceDigest>,
    pub publisher: Option<PublisherIdentity>,
    pub summary: SummaryMetadata,
    pub content_hash: ContentHash,
    pub signature: Signature,
}

impl StoryCapsule {
    pub fn from_story(
        feed_id: impl Into<String>,
        seq: u64,
        author: impl Into<String>,
        story: &CompiledStory,
    ) -> Result<Self, ProtoError> {
        let feed_id = feed_id.into();
        let summaries = summarize_feed(
            &feed_id,
            std::slice::from_ref(story),
            &SummaryConfig::p2p_default(),
        )?;
        let summary = summaries
            .into_iter()
            .next()
            .ok_or(ProtoError::StoryRejected)?;
        Self::from_summary(feed_id, seq, author, &summary)
    }

    pub fn from_summary(
        feed_id: impl Into<String>,
        seq: u64,
        author: impl Into<String>,
        summary: &FeedSummary,
    ) -> Result<Self, ProtoError> {
        let mut capsule = Self {
            capsule_id: format!("cap_{seq:016x}_{}", compact_digest(&summary.headline)),
            feed_id: feed_id.into(),
            compatibility: ProtocolCompatibility::current(),
            seq,
            story_window: summary.story_window.clone(),
            created_at: OffsetDateTime::now_utc(),
            author: author.into(),
            source_agent_kinds: summary.source_agent_kinds.clone(),
            headline: summary.headline.clone(),
            deck: summary.deck.clone(),
            lower_third: summary.lower_third.clone(),
            chips: summary.chips.clone(),
            image: summary.image.clone(),
            story_kind: summary.story_family.into(),
            severity: summary.severity,
            score: summary.score,
            privacy_class: summary.privacy_class,
            evidence: summary
                .evidence_event_ids
                .iter()
                .cloned()
                .map(EvidenceDigest::from_event_id)
                .collect(),
            publisher: None,
            summary: summary.metadata.clone(),
            content_hash: ContentHash(String::new()),
            signature: Signature::unsigned(),
        };
        capsule.content_hash = ContentHash::for_value(&CapsuleContent::from(&capsule))?;
        Ok(capsule)
    }

    pub fn with_publisher(mut self, publisher: PublisherIdentity) -> Result<Self, ProtoError> {
        self.publisher = Some(publisher);
        self.content_hash = ContentHash::for_value(&CapsuleContent::from(&self))?;
        Ok(self)
    }

    pub fn sign(mut self, key_id: &str) -> Result<Self, ProtoError> {
        self.signature = Signature::unsigned();
        self.content_hash = ContentHash::for_value(&CapsuleContent::from(&self))?;
        self.signature = Signature::for_value(key_id, &self)?;
        Ok(self)
    }

    pub fn verify_signature(&self) -> Result<bool, ProtoError> {
        let mut unsigned = self.clone();
        unsigned.signature = Signature::unsigned();
        self.signature.verify_value(&unsigned)
    }

    pub fn sign_with_secret(mut self, key_id: &str, secret: &str) -> Result<Self, ProtoError> {
        self.signature = Signature::unsigned();
        self.content_hash = ContentHash::for_value(&CapsuleContent::from(&self))?;
        self.signature = Signature::for_value_hmac(key_id, secret, &self)?;
        Ok(self)
    }

    pub fn verify_signature_with_secret(&self, secret: &str) -> Result<bool, ProtoError> {
        let mut unsigned = self.clone();
        unsigned.signature = Signature::unsigned();
        self.signature.verify_value_hmac(secret, &unsigned)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FeedEnvelope {
    Clear(Box<Signed<StoryCapsule>>),
    Encrypted(EncryptedCapsule),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EncryptedCapsule {
    pub feed_id: FeedId,
    pub seq: u64,
    pub key_id: String,
    pub ciphertext_digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubscriptionGrant {
    pub feed_id: FeedId,
    pub subscriber: PrincipalRef,
    pub peer_id: PeerIdString,
    pub capabilities: Vec<Capability>,
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub revocation_epoch: u64,
    pub signature: Signature,
}

#[derive(Clone, Debug, Serialize)]
struct CapsuleContent<'a> {
    feed_id: &'a str,
    compatibility: &'a ProtocolCompatibility,
    seq: u64,
    headline: &'a str,
    deck: &'a str,
    lower_third: &'a str,
    chips: &'a [String],
    image: Option<&'a HeadlineImage>,
    score: u8,
    privacy_class: PrivacyClass,
    summary_policy: &'a str,
    summary_processor: &'a str,
    publisher: Option<&'a PublisherIdentity>,
}

impl<'a> From<&'a StoryCapsule> for CapsuleContent<'a> {
    fn from(value: &'a StoryCapsule) -> Self {
        Self {
            feed_id: &value.feed_id,
            compatibility: &value.compatibility,
            seq: value.seq,
            headline: &value.headline,
            deck: &value.deck,
            lower_third: &value.lower_third,
            chips: &value.chips,
            image: value.image.as_ref(),
            score: value.score,
            privacy_class: value.privacy_class,
            summary_policy: &value.summary.policy,
            summary_processor: &value.summary.processor,
            publisher: value.publisher.as_ref(),
        }
    }
}

#[must_use]
pub fn feed_topic(network_id: &str, feed_id: &str) -> String {
    format!(
        "{}.v{}.{network_id}.feed.{}",
        AGENT_FEED_PROTOCOL_NAME,
        AGENT_FEED_PROTOCOL_VERSION,
        compact_digest(feed_id)
    )
}

#[must_use]
pub fn directory_topic(network_id: &str) -> String {
    format!(
        "{}.v{}.{network_id}.directory",
        AGENT_FEED_PROTOCOL_NAME, AGENT_FEED_PROTOCOL_VERSION
    )
}

#[must_use]
pub fn presence_topic(network_id: &str) -> String {
    format!(
        "{}.v{}.{network_id}.presence",
        AGENT_FEED_PROTOCOL_NAME, AGENT_FEED_PROTOCOL_VERSION
    )
}

#[must_use]
pub fn github_user_topic(network_id: &str, github_user_id: u64) -> String {
    format!(
        "{}.v{}.{network_id}.github.{}",
        AGENT_FEED_PROTOCOL_NAME,
        AGENT_FEED_PROTOCOL_VERSION,
        compact_digest(&github_user_id.to_string())
    )
}

#[must_use]
pub fn github_org_topic(network_id: &str, org: &str) -> String {
    format!(
        "{}.v{}.{network_id}.github-org.{}",
        AGENT_FEED_PROTOCOL_NAME,
        AGENT_FEED_PROTOCOL_VERSION,
        compact_digest(&org.to_ascii_lowercase())
    )
}

#[must_use]
pub fn github_team_topic(network_id: &str, org: &str, team: &str) -> String {
    format!(
        "{}.v{}.{network_id}.github-team.{}",
        AGENT_FEED_PROTOCOL_NAME,
        AGENT_FEED_PROTOCOL_VERSION,
        compact_digest(
            format!("{}:{}", org.to_ascii_lowercase(), team.to_ascii_lowercase()).as_str()
        )
    )
}

#[must_use]
pub fn github_provider_key(network_id: &str, github_user_id: u64) -> String {
    format!(
        "/{}/{}/github/{}",
        AGENT_FEED_PROTOCOL_NAME,
        AGENT_FEED_PROTOCOL_VERSION,
        compact_digest(format!("{network_id}:{github_user_id}").as_str())
    )
}

#[must_use]
pub fn github_org_provider_key(network_id: &str, org: &str) -> String {
    format!(
        "/{}/{}/github-org/{}",
        AGENT_FEED_PROTOCOL_NAME,
        AGENT_FEED_PROTOCOL_VERSION,
        compact_digest(format!("{network_id}:{}", org.to_ascii_lowercase()).as_str())
    )
}

#[must_use]
pub fn github_team_provider_key(network_id: &str, org: &str, team: &str) -> String {
    format!(
        "/{}/{}/github-team/{}",
        AGENT_FEED_PROTOCOL_NAME,
        AGENT_FEED_PROTOCOL_VERSION,
        compact_digest(
            format!(
                "{network_id}:{}:{}",
                org.to_ascii_lowercase(),
                team.to_ascii_lowercase()
            )
            .as_str(),
        )
    )
}

#[must_use]
pub fn stable_digest(input: &[u8]) -> String {
    hex_lower(&Sha256::digest(input))
}

#[must_use]
pub fn compact_digest(input: &str) -> String {
    stable_digest(input.as_bytes()).chars().take(16).collect()
}

fn sha256_signature_digest(key_id: &str, payload: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SIGNATURE_DOMAIN);
    hasher.update(b"\0sha256\0");
    hasher.update(key_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(payload);
    format!("sha256:{}", hex_lower(&hasher.finalize()))
}

fn hmac_signature_digest(key_id: &str, secret: &str, payload: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac accepts arbitrary key lengths");
    mac.update(SIGNATURE_DOMAIN);
    mac.update(b"\0hmac-sha256\0");
    mac.update(key_id.as_bytes());
    mac.update(b"\0");
    mac.update(payload);
    format!("hmac-sha256:{}", hex_lower(&mac.finalize().into_bytes()))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (left, right)| acc | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_core::{AgentEvent, EventKind, SourceKind};
    use agent_feed_story::compile_events;

    fn story() -> CompiledStory {
        let mut event = AgentEvent::new(
            SourceKind::Codex,
            EventKind::TestPass,
            "codex verified release tests",
        );
        event.agent = "codex".to_string();
        event.project = Some("agent_feed".to_string());
        event.files = vec!["src/lib.rs".to_string()];
        event.summary = Some("tests passed after the feed publisher update.".to_string());
        event.score_hint = Some(82);
        let mut stories = compile_events([event]);
        stories.remove(0)
    }

    #[test]
    fn low_context_story_is_rejected_without_panicking() {
        let mut event = AgentEvent::new(SourceKind::Codex, EventKind::FileChanged, "patch applied");
        event.agent = "codex".to_string();
        event.project = Some("agent_feed".to_string());
        event.files = vec!["src/lib.rs".to_string()];
        event.summary = Some("1 changed files. raw diff omitted.".to_string());
        event.score_hint = Some(82);
        let stories = compile_events([event]);

        assert!(stories.is_empty());
    }

    #[test]
    fn story_capsule_signature_detects_tamper() -> Result<(), ProtoError> {
        let capsule = StoryCapsule::from_story("feed-workstation", 1, "github:1", &story())?;
        assert_eq!(capsule.compatibility, ProtocolCompatibility::current());
        let signed = Signed::sign_capsule(capsule, "peer-a")?;
        assert!(signed.verify_capsule()?);

        let mut tampered = signed.clone();
        tampered.value.headline = "raw prompt leaked".to_string();
        assert!(!tampered.verify_capsule()?);
        Ok(())
    }

    #[test]
    fn story_capsule_hmac_signature_requires_publish_secret() -> Result<(), ProtoError> {
        let capsule = StoryCapsule::from_story("feed-workstation", 1, "github:1", &story())?;
        let signed = Signed::sign_capsule_with_secret(capsule, "github:123", "session-secret")?;

        assert!(signed.verify_capsule_with_secret("session-secret")?);
        assert!(!signed.verify_capsule()?);
        assert!(!signed.verify_capsule_with_secret("wrong-secret")?);
        assert!(signed.signature.digest.starts_with("hmac-sha256:"));
        Ok(())
    }

    #[test]
    fn story_capsule_rejects_stale_data_model() -> Result<(), ProtoError> {
        let mut capsule = StoryCapsule::from_story("feed-workstation", 1, "github:1", &story())?;
        capsule.compatibility = ProtocolCompatibility::current().with_model_version(1, 1);
        let signed = Signed::sign_capsule(capsule, "peer-a")?;

        assert!(!signed.verify_capsule()?);
        Ok(())
    }

    #[test]
    fn story_capsule_carries_signed_github_publisher() -> Result<(), ProtoError> {
        let publisher = PublisherIdentity::github(
            123,
            "mosure",
            Some("mosure".to_string()),
            Some("/avatar/github/123".to_string()),
        );
        let capsule = StoryCapsule::from_story("feed-workstation", 1, "github:123", &story())?
            .with_publisher(publisher)?;
        let signed = Signed::sign_capsule(capsule, "peer-a")?;

        assert!(signed.verify_capsule()?);
        assert_eq!(
            signed
                .value
                .publisher
                .as_ref()
                .map(PublisherIdentity::display_label),
            Some("@mosure".to_string())
        );
        assert_eq!(
            signed
                .value
                .publisher
                .as_ref()
                .and_then(|publisher| publisher.avatar.as_deref()),
            Some("/avatar/github/123")
        );
        Ok(())
    }

    #[test]
    fn story_capsule_carries_signed_optional_headline_image() -> Result<(), ProtoError> {
        let mut summaries = summarize_feed(
            "feed-workstation",
            std::slice::from_ref(&story()),
            &SummaryConfig::p2p_default(),
        )?;
        let mut summary = summaries.remove(0);
        summary.image = Some(HeadlineImage::new(
            "/assets/headlines/feed-rollup.webp",
            "abstract projection-safe feed recap",
            "test",
        ));
        let capsule = StoryCapsule::from_summary("feed-workstation", 1, "github:123", &summary)?;
        let signed = Signed::sign_capsule(capsule, "peer-a")?;

        assert!(signed.verify_capsule()?);
        assert_eq!(
            signed.value.image.as_ref().map(|image| image.uri.as_str()),
            Some("/assets/headlines/feed-rollup.webp")
        );

        let mut tampered = signed.clone();
        tampered.value.image.as_mut().expect("image exists").alt =
            "raw path /home/mosure/project".to_string();
        assert!(!tampered.verify_capsule()?);
        Ok(())
    }

    #[test]
    fn publisher_tamper_breaks_capsule_signature() -> Result<(), ProtoError> {
        let capsule = StoryCapsule::from_story("feed-workstation", 1, "github:123", &story())?
            .with_publisher(PublisherIdentity::github(123, "mosure", None, None))?;
        let mut signed = Signed::sign_capsule(capsule, "peer-a")?;
        signed
            .value
            .publisher
            .as_mut()
            .expect("publisher exists")
            .github_login = Some("mallory".to_string());

        assert!(!signed.verify_capsule()?);
        Ok(())
    }

    #[test]
    fn feed_topic_hides_raw_feed_id() {
        let topic = feed_topic("agent-feed-mainnet", "github:mosure/workstation");
        assert!(topic.starts_with("agent-feed.v1.agent-feed-mainnet.feed."));
        assert!(!topic.contains("mosure"));
        assert!(!topic.contains("workstation"));
    }

    #[test]
    fn github_user_topic_hides_login_and_numeric_id() {
        let topic = github_user_topic("agent-feed-mainnet", 123_456);
        assert!(topic.starts_with("agent-feed.v1.agent-feed-mainnet.github."));
        assert!(!topic.contains("mosure"));
        assert!(!topic.contains("123456"));
    }

    #[test]
    fn github_org_and_team_topics_hide_names() {
        let org = github_org_topic("agent-feed-mainnet", "aberration-technology");
        let team = github_team_topic("agent-feed-mainnet", "aberration-technology", "release");
        let org_provider = github_org_provider_key("agent-feed-mainnet", "aberration-technology");
        let team_provider =
            github_team_provider_key("agent-feed-mainnet", "aberration-technology", "release");

        for value in [org, team, org_provider, team_provider] {
            assert!(!value.contains("aberration"));
            assert!(!value.contains("release"));
        }
    }

    #[test]
    fn signed_profile_validates() -> Result<(), ProtoError> {
        let profile = FeedProfile::new(
            "feed-a",
            "agent-feed-mainnet",
            "github:1",
            "peer-a",
            "workstation",
            FeedVisibility::Private,
        )
        .sign("peer-a")?;
        assert!(profile.verify_signature()?);
        Ok(())
    }

    #[test]
    fn compatibility_accepts_current_model_overlap() {
        let local = ProtocolCompatibility::current();
        let remote = ProtocolCompatibility::current()
            .with_model_version(AGENT_FEED_MODEL_VERSION, AGENT_FEED_MIN_MODEL_VERSION);

        let status = local.status_with(&remote);

        assert!(status.compatible);
        assert_eq!(status.message, "compatible");
    }

    #[test]
    fn compatibility_rejects_newer_required_model() {
        let local = ProtocolCompatibility::current();
        let remote = ProtocolCompatibility::current()
            .with_model_version(AGENT_FEED_MODEL_VERSION + 1, AGENT_FEED_MODEL_VERSION + 1);

        let status = local.status_with(&remote);

        assert!(!status.compatible);
        assert!(status.message.contains("update your peer"));
    }

    #[test]
    fn compatibility_rejects_protocol_mismatch() {
        let local = ProtocolCompatibility::current();
        let remote =
            ProtocolCompatibility::current().with_protocol_version(AGENT_FEED_PROTOCOL_VERSION + 1);

        let status = local.status_with(&remote);

        assert!(!status.compatible);
        assert!(status.message.contains("protocol changed"));
    }

    #[test]
    fn compatibility_rejects_stale_required_model() {
        let local = ProtocolCompatibility::current();
        let remote = ProtocolCompatibility::current().with_model_version(1, 1);

        let status = local.status_with(&remote);

        assert!(!status.compatible);
        assert!(status.message.contains("older data model"));
    }
}
