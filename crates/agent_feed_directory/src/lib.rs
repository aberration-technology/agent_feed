use agent_feed_core::HeadlineImage;
use agent_feed_identity::{
    GithubLogin, GithubOrgName, GithubTeamSlug, GithubUserId, IdentityError,
};
use agent_feed_identity_github::GithubProfile;
use agent_feed_p2p_proto::{
    AgentKind, FeedId, FeedVisibility, NetworkId, PeerIdString, ProtoError, PublisherIdentity,
    Signature, Signed, StoryCapsule, StoryKind, feed_topic, github_org_topic, github_team_topic,
    github_user_topic, stable_digest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use time::{Duration, OffsetDateTime};

pub type StreamId = String;
pub type AuthorityId = String;
pub type MultiaddrString = String;
pub type RendezvousNamespace = String;
pub type ProviderKey = String;
pub type ProtocolId = String;
pub type TopicId = String;
pub type AvatarRef = String;

#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error("invalid remote route: {0}")]
    InvalidRoute(String),
    #[error("invalid min_score: {0}")]
    InvalidMinScore(String),
    #[error("directory record signature rejected")]
    InvalidSignature,
    #[error("directory record expired")]
    ExpiredRecord,
    #[error("directory record replayed")]
    ReplayedSequence,
    #[error("directory record github id mismatch")]
    GithubIdMismatch,
    #[error("directory record access policy mismatch")]
    AccessPolicyMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteUserRoute {
    pub login: GithubLogin,
    pub network: NetworkSelector,
    pub selection: RemoteFeedSelection,
    pub stream_filter: StreamFilter,
    pub reel_filter: RemoteReelFilter,
    pub layout: ReelLayout,
}

impl RemoteUserRoute {
    pub fn parse(path: &str, query: Option<&str>) -> Result<Self, DirectoryError> {
        let path = path
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();
        if path.is_empty() || path.contains('%') {
            return Err(DirectoryError::InvalidRoute(path.to_string()));
        }
        let segments = path.split('/').collect::<Vec<_>>();
        if segments.len() > 2
            || segments
                .iter()
                .any(|segment| segment.is_empty() || segment.starts_with('.'))
        {
            return Err(DirectoryError::InvalidRoute(path.to_string()));
        }
        let login = GithubLogin::parse(segments[0])?;
        let path_selection = segments
            .get(1)
            .map(|segment| RemoteFeedSelection::from_path_segment(segment))
            .transpose()?
            .unwrap_or_default();
        let params = QueryParams::parse(query.unwrap_or_default());
        let network = params
            .first("network")
            .map(NetworkSelector::from_query)
            .unwrap_or_default();
        let (selection, stream_filter) = if path_selection.is_named_or_wildcard() {
            let filter = path_selection.to_stream_filter();
            (path_selection, filter)
        } else if params.has_flag("all") {
            (RemoteFeedSelection::Wildcard, StreamFilter::all_visible())
        } else if let Some(streams) = params.first("streams") {
            let filter = StreamFilter::from_streams_query(streams, &login)?;
            (RemoteFeedSelection::from_stream_filter(&filter), filter)
        } else {
            let filter = StreamFilter::default_visible();
            (RemoteFeedSelection::DefaultVisible, filter)
        };
        let min_score = params
            .first("min_score")
            .map(|value| {
                value
                    .parse::<u8>()
                    .map_err(|_| DirectoryError::InvalidMinScore(value.to_string()))
            })
            .transpose()?
            .unwrap_or(75);
        let ignored_privacy_params = params.privacy_weakening_keys();
        let mut layout = params
            .first("layout")
            .map(ReelLayout::from_query)
            .unwrap_or_default();
        if params
            .first("view")
            .is_some_and(|value| value == "timeline")
            || params
                .first("timeline")
                .is_some_and(|value| matches!(value, "1" | "true" | "on"))
        {
            layout = ReelLayout::Timeline;
        }
        if params
            .first("mode")
            .is_some_and(|value| value == "incident")
        {
            layout = ReelLayout::Incident;
        }
        Ok(Self {
            login,
            network,
            selection,
            stream_filter,
            reel_filter: RemoteReelFilter {
                agent_kinds: params.first("agents").map(split_csv).unwrap_or_default(),
                story_kinds: params.first("kinds").map(split_csv).unwrap_or_default(),
                min_score,
                story_only: true,
                raw_events: false,
                require_settled: true,
                ignored_privacy_params,
            },
            layout,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkSelector {
    #[default]
    Mainnet,
    Custom(String),
}

impl NetworkSelector {
    #[must_use]
    pub fn network_id(&self) -> NetworkId {
        match self {
            Self::Mainnet => "agent-feed-mainnet".to_string(),
            Self::Custom(value) => value.clone(),
        }
    }

    fn from_query(value: &str) -> Self {
        if value.is_empty() || value == "mainnet" {
            Self::Mainnet
        } else {
            Self::Custom(value.to_string())
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteFeedSelection {
    #[default]
    DefaultVisible,
    Wildcard,
    Named {
        label: String,
    },
}

impl RemoteFeedSelection {
    fn from_path_segment(segment: &str) -> Result<Self, DirectoryError> {
        if segment == "*" {
            return Ok(Self::Wildcard);
        }
        validate_logical_feed_label(segment)?;
        Ok(Self::Named {
            label: segment.to_string(),
        })
    }

    fn from_stream_filter(filter: &StreamFilter) -> Self {
        match filter {
            StreamFilter::AllVisible { .. } => Self::Wildcard,
            StreamFilter::Named { labels, .. } if labels.len() == 1 => Self::Named {
                label: labels[0].clone(),
            },
            _ => Self::DefaultVisible,
        }
    }

    fn is_named_or_wildcard(&self) -> bool {
        matches!(self, Self::Wildcard | Self::Named { .. })
    }

    fn to_stream_filter(&self) -> StreamFilter {
        match self {
            Self::DefaultVisible => StreamFilter::default_visible(),
            Self::Wildcard => StreamFilter::all_visible(),
            Self::Named { label } => StreamFilter::named([label.clone()]),
        }
    }

    #[must_use]
    pub fn route_syntax(&self, login: &GithubLogin) -> String {
        match self {
            Self::DefaultVisible => login.to_string(),
            Self::Wildcard => format!("{login}/*"),
            Self::Named { label } => format!("{login}/{label}"),
        }
    }

    #[must_use]
    pub fn is_wildcard(&self) -> bool {
        matches!(self, Self::Wildcard)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamFilter {
    DefaultVisible {
        story_only: bool,
        raw_events: bool,
        require_settled: bool,
    },
    AllVisible {
        story_only: bool,
        raw_events: bool,
        require_settled: bool,
    },
    Named {
        labels: Vec<String>,
        story_only: bool,
        raw_events: bool,
        require_settled: bool,
    },
}

impl StreamFilter {
    #[must_use]
    pub fn default_visible() -> Self {
        Self::DefaultVisible {
            story_only: true,
            raw_events: false,
            require_settled: true,
        }
    }

    #[must_use]
    pub fn all_visible() -> Self {
        Self::AllVisible {
            story_only: true,
            raw_events: false,
            require_settled: true,
        }
    }

    pub fn named<I>(labels: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        Self::Named {
            labels: labels.into_iter().collect(),
            story_only: true,
            raw_events: false,
            require_settled: true,
        }
    }

    fn from_streams_query(value: &str, login: &GithubLogin) -> Result<Self, DirectoryError> {
        if matches!(value, "all" | "*") || value == format!("{login}/*") {
            return Ok(Self::all_visible());
        }
        let labels = split_stream_labels(value)?;
        if labels.iter().any(|label| label == "*") {
            return Ok(Self::all_visible());
        }
        Ok(Self::named(labels))
    }

    #[must_use]
    pub fn permits_label(&self, label: &str) -> bool {
        match self {
            Self::DefaultVisible { .. } | Self::AllVisible { .. } => true,
            Self::Named { labels, .. } => labels.iter().any(|value| value == label),
        }
    }

    #[must_use]
    pub fn story_only(&self) -> bool {
        match self {
            Self::DefaultVisible { story_only, .. }
            | Self::AllVisible { story_only, .. }
            | Self::Named { story_only, .. } => *story_only,
        }
    }

    #[must_use]
    pub fn raw_events(&self) -> bool {
        match self {
            Self::DefaultVisible { raw_events, .. }
            | Self::AllVisible { raw_events, .. }
            | Self::Named { raw_events, .. } => *raw_events,
        }
    }

    #[must_use]
    pub fn require_settled(&self) -> bool {
        match self {
            Self::DefaultVisible {
                require_settled, ..
            }
            | Self::AllVisible {
                require_settled, ..
            }
            | Self::Named {
                require_settled, ..
            } => *require_settled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteReelFilter {
    pub agent_kinds: Vec<String>,
    pub story_kinds: Vec<String>,
    pub min_score: u8,
    pub story_only: bool,
    pub raw_events: bool,
    pub require_settled: bool,
    pub ignored_privacy_params: Vec<String>,
}

impl Default for RemoteReelFilter {
    fn default() -> Self {
        Self {
            agent_kinds: Vec::new(),
            story_kinds: Vec::new(),
            min_score: 75,
            story_only: true,
            raw_events: false,
            require_settled: true,
            ignored_privacy_params: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReelLayout {
    #[default]
    Stage,
    Timeline,
    Wall,
    Ambient,
    Incident,
    Debug,
}

impl ReelLayout {
    fn from_query(value: &str) -> Self {
        match value {
            "timeline" => Self::Timeline,
            "wall" => Self::Wall,
            "ambient" => Self::Ambient,
            "incident" => Self::Incident,
            "debug" => Self::Debug,
            _ => Self::Stage,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubPrincipal {
    pub github_user_id: GithubUserId,
    pub current_login: String,
    pub display_name: Option<String>,
    pub avatar: Option<AvatarRef>,
    pub verified_by: AuthorityId,
    #[serde(with = "time::serde::rfc3339")]
    pub verified_at: OffsetDateTime,
}

impl GithubPrincipal {
    #[must_use]
    pub fn from_profile(profile: &GithubProfile, authority: impl Into<String>) -> Self {
        Self {
            github_user_id: profile.id,
            current_login: profile.login.to_string(),
            display_name: profile.name.clone(),
            avatar: profile.avatar_url.clone(),
            verified_by: authority.into(),
            verified_at: OffsetDateTime::now_utc(),
        }
    }

    #[must_use]
    pub fn publisher_identity(&self) -> PublisherIdentity {
        PublisherIdentity::github(
            self.github_user_id.get(),
            self.current_login.clone(),
            self.display_name.clone(),
            self.avatar.clone(),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryPolicy {
    pub story_only: bool,
    pub settled_only: bool,
    pub raw_events: bool,
    pub min_score: u8,
}

impl Default for SummaryPolicy {
    fn default() -> Self {
        Self {
            story_only: true,
            settled_only: true,
            raw_events: false,
            min_score: 75,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedAccessPolicy {
    pub github_org: Option<GithubOrgName>,
    pub github_team: Option<GithubTeamSlug>,
    pub github_repo: Option<String>,
    pub github_users: Vec<GithubUserId>,
}

impl FeedAccessPolicy {
    #[must_use]
    pub fn public() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn github_org(org: GithubOrgName) -> Self {
        Self {
            github_org: Some(org),
            github_team: None,
            github_repo: None,
            github_users: Vec::new(),
        }
    }

    #[must_use]
    pub fn github_team(org: GithubOrgName, team: GithubTeamSlug) -> Self {
        Self {
            github_org: Some(org),
            github_team: Some(team),
            github_repo: None,
            github_users: Vec::new(),
        }
    }

    #[must_use]
    pub fn permits_org(&self, org: &GithubOrgName) -> bool {
        self.github_org.as_ref().is_some_and(|value| value == org)
    }

    #[must_use]
    pub fn permits_team(&self, org: &GithubOrgName, team: &GithubTeamSlug) -> bool {
        self.github_org.as_ref().is_some_and(|value| value == org)
            && self.github_team.as_ref().is_some_and(|value| value == team)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamDescriptor {
    pub stream_id: StreamId,
    pub label: String,
    pub agent_kinds: Vec<AgentKind>,
    pub story_kinds: Vec<StoryKind>,
    pub visibility: FeedVisibility,
    pub summary_policy: SummaryPolicy,
    pub access: FeedAccessPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedDirectoryEntry {
    pub feed_id: FeedId,
    pub network_id: NetworkId,
    pub owner: GithubPrincipal,
    pub peer_id: PeerIdString,
    pub feed_label: String,
    pub display_name: String,
    pub avatar: Option<AvatarRef>,
    pub visibility: FeedVisibility,
    pub access: FeedAccessPolicy,
    pub stream_descriptors: Vec<StreamDescriptor>,
    pub rendezvous_namespace: RendezvousNamespace,
    pub live_topic: TopicId,
    pub snapshot_protocol: ProtocolId,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub sequence: u64,
    pub signature: Signature,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalFeedSummary {
    pub owner: GithubPrincipal,
    pub label: String,
    pub route_syntax: String,
    pub feed_ids: Vec<FeedId>,
    pub publisher_peer_ids: Vec<PeerIdString>,
    pub agent_kinds: Vec<AgentKind>,
    pub story_kinds: Vec<String>,
    pub visible_entries: usize,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
}

impl FeedDirectoryEntry {
    #[must_use]
    pub fn new(
        network_id: &str,
        feed_id: impl Into<String>,
        owner: GithubPrincipal,
        peer_id: impl Into<String>,
        feed_label: impl Into<String>,
        visibility: FeedVisibility,
        sequence: u64,
    ) -> Self {
        let feed_id = feed_id.into();
        let feed_label = feed_label.into();
        let now = OffsetDateTime::now_utc();
        let live_topic = feed_topic(network_id, &feed_id);
        let access = FeedAccessPolicy::public();
        Self {
            feed_id: feed_id.clone(),
            network_id: network_id.to_string(),
            owner: owner.clone(),
            peer_id: peer_id.into(),
            display_name: format!(
                "@{} / {}",
                owner.current_login.as_str(),
                feed_label.as_str()
            ),
            avatar: owner.avatar.clone(),
            visibility,
            access: access.clone(),
            stream_descriptors: vec![StreamDescriptor {
                stream_id: stable_digest(format!("{feed_id}:{feed_label}").as_bytes()),
                label: feed_label.clone(),
                agent_kinds: vec!["codex".to_string(), "claude".to_string()],
                story_kinds: vec![StoryKind::Turn, StoryKind::Test, StoryKind::Incident],
                visibility,
                summary_policy: SummaryPolicy::default(),
                access,
            }],
            rendezvous_namespace: github_user_topic(network_id, owner.github_user_id.get()),
            live_topic,
            snapshot_protocol: "/agent-feed/1/snapshot".to_string(),
            last_seen_at: now,
            expires_at: now + Duration::hours(2),
            sequence,
            signature: Signature::unsigned(),
            feed_label,
        }
    }

    pub fn with_github_org(mut self, org: impl AsRef<str>) -> Result<Self, DirectoryError> {
        let org = GithubOrgName::parse(org.as_ref())?;
        self.visibility = FeedVisibility::GithubOrg;
        self.access = FeedAccessPolicy::github_org(org.clone());
        self.rendezvous_namespace = github_org_topic(&self.network_id, org.as_str());
        for stream in &mut self.stream_descriptors {
            stream.visibility = FeedVisibility::GithubOrg;
            stream.access = self.access.clone();
        }
        Ok(self)
    }

    pub fn with_github_team(
        mut self,
        org: impl AsRef<str>,
        team: impl AsRef<str>,
    ) -> Result<Self, DirectoryError> {
        let org = GithubOrgName::parse(org.as_ref())?;
        let team = GithubTeamSlug::parse(team.as_ref())?;
        self.visibility = FeedVisibility::GithubTeam;
        self.access = FeedAccessPolicy::github_team(org.clone(), team.clone());
        self.rendezvous_namespace =
            github_team_topic(&self.network_id, org.as_str(), team.as_str());
        for stream in &mut self.stream_descriptors {
            stream.visibility = FeedVisibility::GithubTeam;
            stream.access = self.access.clone();
        }
        Ok(self)
    }

    pub fn sign(mut self, key_id: &str) -> Result<Self, DirectoryError> {
        self.signature = Signature::unsigned();
        self.signature = Signature::for_value(key_id, &self)?;
        Ok(self)
    }

    pub fn verify_signature(&self) -> Result<bool, DirectoryError> {
        let mut unsigned = self.clone();
        unsigned.signature = Signature::unsigned();
        Ok(Signature::for_value(&self.signature.key_id, &unsigned)? == self.signature)
    }

    #[must_use]
    pub fn access_matches_visibility(&self) -> bool {
        let entry_matches = match self.visibility {
            FeedVisibility::GithubOrg => {
                self.access.github_org.is_some() && self.access.github_team.is_none()
            }
            FeedVisibility::GithubTeam => {
                self.access.github_org.is_some() && self.access.github_team.is_some()
            }
            _ => true,
        };
        entry_matches
            && self.stream_descriptors.iter().all(|stream| {
                stream.visibility == self.visibility
                    && match stream.visibility {
                        FeedVisibility::GithubOrg => {
                            stream.access.github_org.is_some()
                                && stream.access.github_team.is_none()
                        }
                        FeedVisibility::GithubTeam => {
                            stream.access.github_org.is_some()
                                && stream.access.github_team.is_some()
                        }
                        _ => true,
                    }
            })
    }

    #[must_use]
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        self.expires_at <= now
    }

    #[must_use]
    pub fn is_publicly_visible(&self) -> bool {
        self.visibility == FeedVisibility::Public
    }

    #[must_use]
    pub fn publisher_identity(&self) -> PublisherIdentity {
        self.owner.publisher_identity()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHeadlineView {
    pub feed_id: FeedId,
    pub feed_label: String,
    pub publisher_login: String,
    pub publisher_display_name: Option<String>,
    pub publisher_avatar: Option<AvatarRef>,
    pub verified: bool,
    pub headline: String,
    pub deck: String,
    pub lower_third: String,
    pub chips: Vec<String>,
    pub image: Option<HeadlineImage>,
}

impl RemoteHeadlineView {
    pub fn from_entry_and_capsule(
        entry: &FeedDirectoryEntry,
        capsule: &Signed<StoryCapsule>,
    ) -> Result<Self, DirectoryError> {
        if !capsule.verify_capsule()? {
            return Err(DirectoryError::InvalidSignature);
        }
        if capsule.value.feed_id != entry.feed_id {
            return Err(DirectoryError::GithubIdMismatch);
        }
        if let Some(publisher) = capsule.value.publisher.as_ref()
            && publisher.github_user_id != Some(entry.owner.github_user_id.get())
        {
            return Err(DirectoryError::GithubIdMismatch);
        }
        let publisher = capsule
            .value
            .publisher
            .clone()
            .unwrap_or_else(|| entry.publisher_identity());
        Ok(Self {
            feed_id: entry.feed_id.clone(),
            feed_label: entry.feed_label.clone(),
            publisher_login: publisher
                .github_login
                .clone()
                .unwrap_or_else(|| entry.owner.current_login.clone()),
            publisher_display_name: publisher
                .display_name
                .clone()
                .or_else(|| entry.owner.display_name.clone()),
            publisher_avatar: publisher.avatar.clone().or_else(|| entry.avatar.clone()),
            verified: publisher.verified,
            headline: capsule.value.headline.clone(),
            deck: capsule.value.deck.clone(),
            lower_third: format!(
                "{} / {}",
                publisher.display_label(),
                entry.feed_label.as_str()
            ),
            chips: capsule.value.chips.clone(),
            image: capsule.value.image.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubProfileView {
    pub login: String,
    pub name: Option<String>,
    pub avatar: Option<String>,
}

impl From<&GithubProfile> for GithubProfileView {
    fn from(value: &GithubProfile) -> Self {
        Self {
            login: value.login.to_string(),
            name: value.name.clone(),
            avatar: value.avatar_url.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedBrowserSeed {
    pub network_id: NetworkId,
    pub edge_base_url: String,
    pub bootstrap_peers: Vec<MultiaddrString>,
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub signature: Signature,
}

impl SignedBrowserSeed {
    pub fn new(
        network_id: impl Into<String>,
        edge_base_url: impl Into<String>,
        bootstrap_peers: Vec<MultiaddrString>,
        key_id: &str,
    ) -> Result<Self, DirectoryError> {
        let now = OffsetDateTime::now_utc();
        let mut seed = Self {
            network_id: network_id.into(),
            edge_base_url: edge_base_url.into(),
            bootstrap_peers,
            issued_at: now,
            expires_at: now + Duration::minutes(15),
            signature: Signature::unsigned(),
        };
        seed.signature = Signature::for_value(key_id, &seed)?;
        Ok(seed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubDiscoveryTicket {
    pub network_id: NetworkId,
    pub requested_login: GithubLogin,
    pub resolved_github_id: GithubUserId,
    pub profile: GithubProfileView,
    pub candidate_feeds: Vec<FeedDirectoryEntry>,
    pub bootstrap_peers: Vec<MultiaddrString>,
    pub rendezvous_namespaces: Vec<RendezvousNamespace>,
    pub provider_keys: Vec<ProviderKey>,
    pub browser_seed: SignedBrowserSeed,
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub signature: Signature,
}

impl GithubDiscoveryTicket {
    pub fn sign(mut self, key_id: &str) -> Result<Self, DirectoryError> {
        self.signature = Signature::unsigned();
        self.signature = Signature::for_value(key_id, &self)?;
        Ok(self)
    }

    pub fn verify_signature(&self) -> Result<bool, DirectoryError> {
        let mut unsigned = self.clone();
        unsigned.signature = Signature::unsigned();
        Ok(Signature::for_value(&self.signature.key_id, &unsigned)? == self.signature)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgDiscoveryTicket {
    pub network_id: NetworkId,
    pub org: GithubOrgName,
    pub team: Option<GithubTeamSlug>,
    pub candidate_feeds: Vec<FeedDirectoryEntry>,
    pub bootstrap_peers: Vec<MultiaddrString>,
    pub rendezvous_namespaces: Vec<RendezvousNamespace>,
    pub provider_keys: Vec<ProviderKey>,
    pub browser_seed: SignedBrowserSeed,
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub signature: Signature,
}

impl OrgDiscoveryTicket {
    pub fn sign(mut self, key_id: &str) -> Result<Self, DirectoryError> {
        self.signature = Signature::unsigned();
        self.signature = Signature::for_value(key_id, &self)?;
        Ok(self)
    }

    pub fn verify_signature(&self) -> Result<bool, DirectoryError> {
        let mut unsigned = self.clone();
        unsigned.signature = Signature::unsigned();
        Ok(Signature::for_value(&self.signature.key_id, &unsigned)? == self.signature)
    }
}

#[derive(Clone, Debug, Default)]
pub struct DirectoryStore {
    entries: BTreeMap<FeedId, FeedDirectoryEntry>,
    highest_sequence: BTreeMap<FeedId, u64>,
}

impl DirectoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&mut self, entry: FeedDirectoryEntry) -> Result<(), DirectoryError> {
        if !entry.verify_signature()? {
            return Err(DirectoryError::InvalidSignature);
        }
        if !entry.access_matches_visibility() {
            return Err(DirectoryError::AccessPolicyMismatch);
        }
        if entry.is_expired(OffsetDateTime::now_utc()) {
            return Err(DirectoryError::ExpiredRecord);
        }
        if self
            .highest_sequence
            .get(&entry.feed_id)
            .is_some_and(|sequence| *sequence >= entry.sequence)
        {
            return Err(DirectoryError::ReplayedSequence);
        }
        self.highest_sequence
            .insert(entry.feed_id.clone(), entry.sequence);
        self.entries.insert(entry.feed_id.clone(), entry);
        Ok(())
    }

    #[must_use]
    pub fn entries_for_github_user(&self, github_user_id: GithubUserId) -> Vec<FeedDirectoryEntry> {
        self.entries
            .values()
            .filter(|entry| entry.owner.github_user_id == github_user_id)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn entries_for_github_org(&self, org: &GithubOrgName) -> Vec<FeedDirectoryEntry> {
        self.entries
            .values()
            .filter(|entry| entry.access.permits_org(org))
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn entries_for_github_team(
        &self,
        org: &GithubOrgName,
        team: &GithubTeamSlug,
    ) -> Vec<FeedDirectoryEntry> {
        self.entries
            .values()
            .filter(|entry| entry.access.permits_team(org, team))
            .cloned()
            .collect()
    }

    pub fn visible_entries_for_route(
        &self,
        github_user_id: GithubUserId,
        route: &RemoteUserRoute,
    ) -> Result<Vec<FeedDirectoryEntry>, DirectoryError> {
        let mut entries = Vec::new();
        for entry in self.entries_for_github_user(github_user_id) {
            if entry.owner.github_user_id != github_user_id {
                return Err(DirectoryError::GithubIdMismatch);
            }
            if !route.stream_filter.permits_label(&entry.feed_label) {
                continue;
            }
            if !entry.is_publicly_visible() {
                continue;
            }
            entries.push(filter_entry_streams(entry, route));
        }
        Ok(entries)
    }

    pub fn visible_entries_for_org(
        &self,
        org: &GithubOrgName,
        team: Option<&GithubTeamSlug>,
        filter: &OrgRouteFilter,
    ) -> Result<Vec<FeedDirectoryEntry>, DirectoryError> {
        let entries = if let Some(team) = team {
            self.entries_for_github_team(org, team)
        } else {
            self.entries_for_github_org(org)
        };
        let mut visible = Vec::new();
        for entry in entries {
            if !filter.stream_filter.permits_label(&entry.feed_label) {
                continue;
            }
            if !entry_is_org_visible(&entry, org, team) {
                continue;
            }
            visible.push(filter_entry_streams_for_org(entry, filter));
        }
        Ok(visible)
    }

    #[must_use]
    pub fn logical_feeds_for_github_user(
        &self,
        github_user_id: GithubUserId,
    ) -> Vec<LogicalFeedSummary> {
        let mut groups = BTreeMap::<String, Vec<FeedDirectoryEntry>>::new();
        for entry in self.entries_for_github_user(github_user_id) {
            groups
                .entry(entry.feed_label.clone())
                .or_default()
                .push(entry);
        }
        groups
            .into_iter()
            .filter_map(|(label, entries)| logical_feed_summary(label, entries))
            .collect()
    }

    pub fn visible_logical_feeds_for_route(
        &self,
        github_user_id: GithubUserId,
        route: &RemoteUserRoute,
    ) -> Result<Vec<LogicalFeedSummary>, DirectoryError> {
        let entries = self.visible_entries_for_route(github_user_id, route)?;
        let mut groups = BTreeMap::<String, Vec<FeedDirectoryEntry>>::new();
        for entry in entries {
            groups
                .entry(entry.feed_label.clone())
                .or_default()
                .push(entry);
        }
        Ok(groups
            .into_iter()
            .filter_map(|(label, entries)| logical_feed_summary(label, entries))
            .collect())
    }

    pub fn visible_logical_feeds_for_org(
        &self,
        org: &GithubOrgName,
        team: Option<&GithubTeamSlug>,
        filter: &OrgRouteFilter,
    ) -> Result<Vec<LogicalFeedSummary>, DirectoryError> {
        let entries = self.visible_entries_for_org(org, team, filter)?;
        let mut groups = BTreeMap::<String, Vec<FeedDirectoryEntry>>::new();
        for entry in entries {
            groups
                .entry(entry.feed_label.clone())
                .or_default()
                .push(entry);
        }
        Ok(groups
            .into_iter()
            .filter_map(|(label, entries)| logical_feed_summary(label, entries))
            .collect())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgRouteFilter {
    pub stream_filter: StreamFilter,
    pub reel_filter: RemoteReelFilter,
}

impl OrgRouteFilter {
    #[must_use]
    pub fn all_visible() -> Self {
        Self {
            stream_filter: StreamFilter::all_visible(),
            reel_filter: RemoteReelFilter::default(),
        }
    }

    pub fn from_query(query: Option<&str>) -> Result<Self, DirectoryError> {
        let params = QueryParams::parse(query.unwrap_or_default());
        let stream_filter = if params.has_flag("all") {
            StreamFilter::all_visible()
        } else if let Some(streams) = params.first("streams") {
            if matches!(streams, "all" | "*") {
                StreamFilter::all_visible()
            } else {
                StreamFilter::named(split_stream_labels(streams)?)
            }
        } else {
            StreamFilter::default_visible()
        };
        let min_score = params
            .first("min_score")
            .map(|value| {
                value
                    .parse::<u8>()
                    .map_err(|_| DirectoryError::InvalidMinScore(value.to_string()))
            })
            .transpose()?
            .unwrap_or(75);
        Ok(Self {
            stream_filter,
            reel_filter: RemoteReelFilter {
                agent_kinds: params.first("agents").map(split_csv).unwrap_or_default(),
                story_kinds: params.first("kinds").map(split_csv).unwrap_or_default(),
                min_score,
                story_only: true,
                raw_events: false,
                require_settled: true,
                ignored_privacy_params: params.privacy_weakening_keys(),
            },
        })
    }
}

fn logical_feed_summary(
    label: String,
    mut entries: Vec<FeedDirectoryEntry>,
) -> Option<LogicalFeedSummary> {
    entries.sort_by(|left, right| {
        left.last_seen_at
            .cmp(&right.last_seen_at)
            .then_with(|| left.feed_id.cmp(&right.feed_id))
    });
    let owner = entries.first()?.owner.clone();
    let mut feed_ids = BTreeSet::new();
    let mut publisher_peer_ids = BTreeSet::new();
    let mut agent_kinds = BTreeSet::new();
    let mut story_kinds = BTreeSet::new();
    let mut last_seen_at = entries[0].last_seen_at;
    for entry in &entries {
        feed_ids.insert(entry.feed_id.clone());
        publisher_peer_ids.insert(entry.peer_id.clone());
        if entry.last_seen_at > last_seen_at {
            last_seen_at = entry.last_seen_at;
        }
        for stream in &entry.stream_descriptors {
            agent_kinds.extend(stream.agent_kinds.iter().cloned());
            story_kinds.extend(stream.story_kinds.iter().map(story_kind_label));
        }
    }
    Some(LogicalFeedSummary {
        route_syntax: format!("{}/{}", owner.current_login, label),
        owner,
        label,
        feed_ids: feed_ids.into_iter().collect(),
        publisher_peer_ids: publisher_peer_ids.into_iter().collect(),
        agent_kinds: agent_kinds.into_iter().collect(),
        story_kinds: story_kinds.into_iter().collect(),
        visible_entries: entries.len(),
        last_seen_at,
    })
}

fn filter_entry_streams(
    mut entry: FeedDirectoryEntry,
    route: &RemoteUserRoute,
) -> FeedDirectoryEntry {
    entry.stream_descriptors.retain(|stream| {
        route.stream_filter.permits_label(&stream.label)
            && stream.summary_policy.story_only
            && stream.summary_policy.settled_only
            && !stream.summary_policy.raw_events
            && route_agents_match(stream, route)
            && route_kinds_match(stream, route)
    });
    entry
}

fn filter_entry_streams_for_org(
    mut entry: FeedDirectoryEntry,
    filter: &OrgRouteFilter,
) -> FeedDirectoryEntry {
    entry.stream_descriptors.retain(|stream| {
        filter.stream_filter.permits_label(&stream.label)
            && stream.summary_policy.story_only
            && stream.summary_policy.settled_only
            && !stream.summary_policy.raw_events
            && org_filter_agents_match(stream, filter)
            && org_filter_kinds_match(stream, filter)
    });
    entry
}

fn entry_is_org_visible(
    entry: &FeedDirectoryEntry,
    org: &GithubOrgName,
    team: Option<&GithubTeamSlug>,
) -> bool {
    match (entry.visibility, team) {
        (FeedVisibility::GithubOrg, None) => entry.access.permits_org(org),
        (FeedVisibility::GithubOrg, Some(_)) => entry.access.permits_org(org),
        (FeedVisibility::GithubTeam, Some(team)) => entry.access.permits_team(org, team),
        (FeedVisibility::Public, _) => true,
        _ => false,
    }
}

fn route_agents_match(stream: &StreamDescriptor, route: &RemoteUserRoute) -> bool {
    route.reel_filter.agent_kinds.is_empty()
        || stream.agent_kinds.iter().any(|agent| {
            route
                .reel_filter
                .agent_kinds
                .iter()
                .any(|want| want == agent)
        })
}

fn route_kinds_match(stream: &StreamDescriptor, route: &RemoteUserRoute) -> bool {
    if route.reel_filter.story_kinds.is_empty() {
        return true;
    }
    route.reel_filter.story_kinds.iter().any(|requested| {
        stream
            .story_kinds
            .iter()
            .any(|available| story_kind_matches(*available, requested))
    })
}

fn org_filter_agents_match(stream: &StreamDescriptor, filter: &OrgRouteFilter) -> bool {
    filter.reel_filter.agent_kinds.is_empty()
        || stream.agent_kinds.iter().any(|agent| {
            filter
                .reel_filter
                .agent_kinds
                .iter()
                .any(|want| want == agent)
        })
}

fn org_filter_kinds_match(stream: &StreamDescriptor, filter: &OrgRouteFilter) -> bool {
    if filter.reel_filter.story_kinds.is_empty() {
        return true;
    }
    filter.reel_filter.story_kinds.iter().any(|requested| {
        stream
            .story_kinds
            .iter()
            .any(|available| story_kind_matches(*available, requested))
    })
}

fn story_kind_matches(available: StoryKind, requested: &str) -> bool {
    let requested = requested.to_ascii_lowercase();
    match available {
        StoryKind::Turn => matches!(
            requested.as_str(),
            "turn" | "turn.start" | "turn.complete" | "turn.fail"
        ),
        StoryKind::Plan => matches!(requested.as_str(), "plan" | "plan.update"),
        StoryKind::Test => matches!(requested.as_str(), "test" | "test.pass" | "test.fail"),
        StoryKind::Permission => matches!(
            requested.as_str(),
            "permission" | "permission.request" | "permission.denied"
        ),
        StoryKind::Command => matches!(requested.as_str(), "command" | "command.exec"),
        StoryKind::FileChange => matches!(
            requested.as_str(),
            "file-change" | "file.changed" | "diff.created"
        ),
        StoryKind::Mcp => matches!(requested.as_str(), "mcp" | "mcp.call" | "mcp.fail"),
        StoryKind::Incident => requested == "incident",
        StoryKind::Recap => matches!(requested.as_str(), "recap" | "summary.created"),
    }
}

#[derive(Default)]
struct QueryParams(Vec<(String, String)>);

impl QueryParams {
    fn parse(query: &str) -> Self {
        Self(
            query
                .split('&')
                .filter(|part| !part.is_empty())
                .map(|part| {
                    part.split_once('=')
                        .map(|(key, value)| (key.to_string(), value.to_string()))
                        .unwrap_or_else(|| (part.to_string(), String::new()))
                })
                .collect(),
        )
    }

    fn first(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    fn has_flag(&self, key: &str) -> bool {
        self.0
            .iter()
            .any(|(candidate, value)| candidate == key && value.is_empty())
    }

    fn privacy_weakening_keys(&self) -> Vec<String> {
        self.0
            .iter()
            .filter(|(key, value)| {
                matches!(
                    (key.as_str(), value.as_str()),
                    ("redact", "off")
                        | ("redaction", "off")
                        | ("raw", "true")
                        | ("raw_events", "true")
                        | ("diffs", "true")
                        | ("prompts", "true")
                        | ("command_output", "true")
                )
            })
            .map(|(key, _)| key.clone())
            .collect()
    }
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn split_stream_labels(value: &str) -> Result<Vec<String>, DirectoryError> {
    let labels = split_csv(value);
    if labels.is_empty() {
        return Err(DirectoryError::InvalidRoute(
            "empty streams filter".to_string(),
        ));
    }
    for label in &labels {
        if label != "*" {
            validate_logical_feed_label(label)?;
        }
    }
    Ok(labels)
}

fn validate_logical_feed_label(value: &str) -> Result<(), DirectoryError> {
    if is_valid_logical_feed_label(value) {
        Ok(())
    } else {
        Err(DirectoryError::InvalidRoute(format!(
            "invalid feed label: {value}"
        )))
    }
}

fn is_valid_logical_feed_label(value: &str) -> bool {
    let len = value.len();
    len > 0
        && len <= 64
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn story_kind_label(kind: &StoryKind) -> String {
    match kind {
        StoryKind::Turn => "turn",
        StoryKind::Plan => "plan",
        StoryKind::Test => "test",
        StoryKind::Permission => "permission",
        StoryKind::Command => "command",
        StoryKind::FileChange => "file-change",
        StoryKind::Mcp => "mcp",
        StoryKind::Incident => "incident",
        StoryKind::Recap => "recap",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_core::{AgentEvent, EventKind, SourceKind};
    use agent_feed_p2p_proto::StoryCapsule;
    use agent_feed_story::compile_events;

    fn profile(login: &str, id: u64) -> GithubProfile {
        GithubProfile {
            id: GithubUserId::new(id),
            login: GithubLogin::parse(login).expect("login parses"),
            name: Some(login.to_string()),
            avatar_url: Some(format!("/avatar/github/{id}")),
        }
    }

    fn entry(label: &str, visibility: FeedVisibility, sequence: u64) -> FeedDirectoryEntry {
        FeedDirectoryEntry::new(
            "agent-feed-mainnet",
            format!("feed-{label}"),
            GithubPrincipal::from_profile(&profile("mosure", 123), "edge"),
            "peer-a",
            label,
            visibility,
            sequence,
        )
        .sign("peer-a")
        .expect("entry signs")
    }

    fn org_entry(
        login: &str,
        user_id: u64,
        feed_id: &str,
        label: &str,
        peer_id: &str,
        team: Option<&str>,
    ) -> FeedDirectoryEntry {
        let owner = GithubPrincipal::from_profile(&profile(login, user_id), "edge");
        let entry = FeedDirectoryEntry::new(
            "agent-feed-mainnet",
            feed_id,
            owner,
            peer_id,
            label,
            FeedVisibility::GithubOrg,
            1,
        );
        let entry = if let Some(team) = team {
            entry
                .with_github_team("aberration-technology", team)
                .expect("team policy applies")
        } else {
            entry
                .with_github_org("aberration-technology")
                .expect("org policy applies")
        };
        entry.sign(peer_id).expect("entry signs")
    }

    fn signed_capsule(feed_id: &str, entry: &FeedDirectoryEntry) -> Signed<StoryCapsule> {
        let mut event = AgentEvent::new(
            SourceKind::Codex,
            EventKind::TurnComplete,
            "release triage finished",
        );
        event.agent = "codex".to_string();
        event.score_hint = Some(84);
        let story = compile_events([event]).remove(0);
        let capsule = StoryCapsule::from_story(feed_id, 1, "github:123", &story)
            .expect("capsule builds")
            .with_publisher(entry.publisher_identity())
            .expect("publisher attaches");
        Signed::sign_capsule(capsule, "peer-a").expect("capsule signs")
    }

    #[test]
    fn route_parser_accepts_username_forms_and_all() {
        let route = RemoteUserRoute::parse("/@mosure", Some("all")).expect("route parses");
        assert_eq!(route.login.as_str(), "mosure");
        assert_eq!(route.selection.route_syntax(&route.login), "mosure/*");
        assert!(matches!(
            route.stream_filter,
            StreamFilter::AllVisible {
                story_only: true,
                raw_events: false,
                require_settled: true
            }
        ));
    }

    #[test]
    fn route_parser_accepts_logical_feed_and_wildcard_paths() {
        let named = RemoteUserRoute::parse("/mosure/workstation", Some("view=timeline"))
            .expect("named route parses");
        assert_eq!(
            named.selection.route_syntax(&named.login),
            "mosure/workstation"
        );
        assert_eq!(named.layout, ReelLayout::Timeline);
        assert!(matches!(named.stream_filter, StreamFilter::Named { .. }));

        let wildcard = RemoteUserRoute::parse("/@mosure/*", None).expect("wildcard route parses");
        assert_eq!(wildcard.selection.route_syntax(&wildcard.login), "mosure/*");
        assert!(wildcard.selection.is_wildcard());
        assert!(matches!(
            wildcard.stream_filter,
            StreamFilter::AllVisible { .. }
        ));
    }

    #[test]
    fn route_parser_accepts_filters_and_ignores_privacy_weakening_params() {
        let route = RemoteUserRoute::parse(
            "/mosure",
            Some("streams=workstation,release&agents=codex,claude&kinds=turn.complete,test.fail&min_score=75&redact=off&raw=true"),
        )
        .expect("route parses");
        assert_eq!(route.reel_filter.agent_kinds, vec!["codex", "claude"]);
        assert_eq!(route.reel_filter.min_score, 75);
        assert!(!route.reel_filter.raw_events);
        assert_eq!(
            route.reel_filter.ignored_privacy_params,
            vec!["redact", "raw"]
        );
        assert!(matches!(route.stream_filter, StreamFilter::Named { .. }));
    }

    #[test]
    fn route_parser_rejects_invalid_paths() {
        assert!(RemoteUserRoute::parse("/foo/bar/baz", None).is_err());
        assert!(RemoteUserRoute::parse("/.env", None).is_err());
        assert!(RemoteUserRoute::parse("/mosure/.env", None).is_err());
        assert!(RemoteUserRoute::parse("/%2e%2e", None).is_err());
        assert!(RemoteUserRoute::parse("/mosure/secrets/key", None).is_err());
        assert!(RemoteUserRoute::parse("/mosure/bad label", None).is_err());
    }

    #[test]
    fn signed_directory_record_verifies_and_replay_is_rejected() {
        let first = entry("workstation", FeedVisibility::Public, 1);
        let mut store = DirectoryStore::new();
        store.publish(first.clone()).expect("publish succeeds");
        assert!(first.verify_signature().expect("signature checks"));
        assert!(matches!(
            store.publish(first),
            Err(DirectoryError::ReplayedSequence)
        ));
    }

    #[test]
    fn org_visibility_requires_matching_access_policy() {
        let bad = FeedDirectoryEntry::new(
            "agent-feed-mainnet",
            "feed-org",
            GithubPrincipal::from_profile(&profile("mosure", 123), "edge"),
            "peer-a",
            "workstation",
            FeedVisibility::GithubOrg,
            1,
        )
        .sign("peer-a")
        .expect("entry signs");
        let mut store = DirectoryStore::new();

        assert!(matches!(
            store.publish(bad),
            Err(DirectoryError::AccessPolicyMismatch)
        ));
    }

    #[test]
    fn all_visible_returns_public_settled_story_streams_only() {
        let mut store = DirectoryStore::new();
        store
            .publish(entry("workstation", FeedVisibility::Public, 1))
            .expect("public publish succeeds");
        store
            .publish(entry("private", FeedVisibility::Private, 1))
            .expect("private publish succeeds");
        let route = RemoteUserRoute::parse("/mosure", Some("all")).expect("route parses");
        let entries = store
            .visible_entries_for_route(GithubUserId::new(123), &route)
            .expect("entries filter");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].feed_label, "workstation");
        assert!(entries[0].stream_descriptors[0].summary_policy.story_only);
        assert!(!entries[0].stream_descriptors[0].summary_policy.raw_events);
    }

    #[test]
    fn org_visible_entries_include_many_users_and_many_peers() {
        let mut store = DirectoryStore::new();
        store
            .publish(org_entry(
                "mosure",
                123,
                "feed-mosure-workstation",
                "workstation",
                "peer-a",
                None,
            ))
            .expect("first org feed publishes");
        store
            .publish(org_entry(
                "alice",
                456,
                "feed-alice-release",
                "release",
                "peer-b",
                None,
            ))
            .expect("second org feed publishes");
        store
            .publish(org_entry(
                "mosure",
                123,
                "feed-mosure-release-node-2",
                "release",
                "peer-c",
                None,
            ))
            .expect("third org feed publishes");
        store
            .publish(entry("public", FeedVisibility::Public, 1))
            .expect("public feed publishes");

        let filter = OrgRouteFilter::from_query(Some("all")).expect("filter parses");
        let entries = store
            .visible_entries_for_org(
                &GithubOrgName::parse("aberration-technology").expect("org parses"),
                None,
                &filter,
            )
            .expect("org entries filter");

        assert_eq!(entries.len(), 3);
        assert!(entries.iter().any(|entry| entry.peer_id == "peer-a"));
        assert!(entries.iter().any(|entry| entry.peer_id == "peer-b"));
        assert!(entries.iter().any(|entry| entry.peer_id == "peer-c"));
        assert!(
            entries
                .iter()
                .all(|entry| entry.access.github_org.is_some())
        );
    }

    #[test]
    fn team_visible_entries_are_narrower_than_org_entries() {
        let mut store = DirectoryStore::new();
        store
            .publish(org_entry(
                "mosure",
                123,
                "feed-release",
                "release",
                "peer-a",
                Some("release"),
            ))
            .expect("team feed publishes");
        store
            .publish(org_entry("alice", 456, "feed-lab", "lab", "peer-b", None))
            .expect("org feed publishes");

        let filter = OrgRouteFilter::from_query(Some("all")).expect("filter parses");
        let org = GithubOrgName::parse("aberration-technology").expect("org parses");
        let release = GithubTeamSlug::parse("release").expect("team parses");
        let team_entries = store
            .visible_entries_for_org(&org, Some(&release), &filter)
            .expect("team entries filter");

        assert_eq!(team_entries.len(), 1);
        assert_eq!(team_entries[0].feed_label, "release");
        assert!(team_entries[0].access.permits_team(&org, &release));
    }

    #[test]
    fn logical_feed_summaries_group_multiple_nodes_under_one_name() {
        let mut store = DirectoryStore::new();
        let mut first = entry("release", FeedVisibility::Public, 1);
        first.feed_id = "feed-release-a".to_string();
        first.peer_id = "peer-a".to_string();
        first = first.sign("peer-a").expect("first resigns");
        let mut second = entry("release", FeedVisibility::Public, 1);
        second.feed_id = "feed-release-b".to_string();
        second.peer_id = "peer-b".to_string();
        second = second.sign("peer-b").expect("second resigns");
        store.publish(first).expect("first publishes");
        store.publish(second).expect("second publishes");

        let route = RemoteUserRoute::parse("/mosure/release", None).expect("route parses");
        let summaries = store
            .visible_logical_feeds_for_route(GithubUserId::new(123), &route)
            .expect("logical feeds filter");

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].label, "release");
        assert_eq!(summaries[0].route_syntax, "mosure/release");
        assert_eq!(summaries[0].feed_ids.len(), 2);
        assert_eq!(summaries[0].publisher_peer_ids, vec!["peer-a", "peer-b"]);
        assert_eq!(summaries[0].visible_entries, 2);
    }

    #[test]
    fn directory_topics_do_not_include_github_login() {
        let record = entry("workstation", FeedVisibility::Public, 1);
        assert!(!record.rendezvous_namespace.contains("mosure"));
        assert!(!record.live_topic.contains("mosure"));
        assert!(!record.live_topic.contains("workstation"));
    }

    #[test]
    fn remote_headline_view_uses_verified_github_publisher_avatar() {
        let entry = entry("workstation", FeedVisibility::Public, 1);
        let capsule = signed_capsule("feed-workstation", &entry);

        let view =
            RemoteHeadlineView::from_entry_and_capsule(&entry, &capsule).expect("view builds");

        assert_eq!(view.publisher_login, "mosure");
        assert_eq!(view.publisher_avatar.as_deref(), Some("/avatar/github/123"));
        assert!(view.verified);
        assert_eq!(view.lower_third, "@mosure / workstation");
    }

    #[test]
    fn remote_headline_view_preserves_optional_headline_image() {
        let entry = entry("workstation", FeedVisibility::Public, 1);
        let mut capsule = signed_capsule("feed-workstation", &entry).value;
        capsule.image = Some(HeadlineImage::new(
            "/assets/headlines/release.webp",
            "abstract release signal",
            "test",
        ));
        let signed = Signed::sign_capsule(capsule, "peer-a").expect("capsule signs");

        let view =
            RemoteHeadlineView::from_entry_and_capsule(&entry, &signed).expect("view builds");

        assert_eq!(
            view.image.as_ref().map(|image| image.uri.as_str()),
            Some("/assets/headlines/release.webp")
        );
    }

    #[test]
    fn remote_headline_view_rejects_wrong_github_publisher() {
        let entry = entry("workstation", FeedVisibility::Public, 1);
        let mut capsule = signed_capsule("feed-workstation", &entry);
        capsule
            .value
            .publisher
            .as_mut()
            .expect("publisher exists")
            .github_user_id = Some(999);
        capsule = Signed::sign_capsule(capsule.value, "peer-a").expect("capsule resigns");

        assert!(matches!(
            RemoteHeadlineView::from_entry_and_capsule(&entry, &capsule),
            Err(DirectoryError::GithubIdMismatch)
        ));
    }
}
