use agent_feed_auth_github::browser_sign_in_url;
use agent_feed_core::HeadlineImage;
use agent_feed_directory::{
    DirectoryError, FeedDirectoryEntry, GithubDiscoveryTicket, RemoteHeadlineView, RemoteUserRoute,
};
use agent_feed_p2p_proto::{Signed, StoryCapsule};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use time::OffsetDateTime;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteFeedMode {
    #[default]
    Discovery,
    Subscribed,
}

impl RemoteFeedMode {
    #[must_use]
    pub fn from_query(query: &str) -> Self {
        let params = QueryPairs::parse(query);
        let explicit = params
            .first("feed_mode")
            .or_else(|| params.first("feedMode"))
            .or_else(|| params.first("source"))
            .unwrap_or_default();
        if matches!(explicit, "subscribed" | "subscriptions" | "following")
            || params.has("subscriptions")
            || params.has("subscribed")
            || params
                .first("following")
                .is_some_and(|value| matches!(value, "1" | "true" | "on"))
        {
            Self::Subscribed
        } else {
            Self::Discovery
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteOperatingMode {
    pub mode: RemoteFeedMode,
    pub subscription_targets: Vec<String>,
}

impl RemoteOperatingMode {
    #[must_use]
    pub fn from_query(selection_syntax: impl Into<String>, query: &str) -> Self {
        let selection_syntax = selection_syntax.into();
        let params = QueryPairs::parse(query);
        let targets = params
            .first("subscriptions")
            .or_else(|| params.first("subscribed"))
            .or_else(|| params.first("following"))
            .map(split_targets)
            .unwrap_or_else(|| vec![selection_syntax]);
        Self {
            mode: RemoteFeedMode::from_query(query),
            subscription_targets: targets,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRouteState {
    #[default]
    ParsingRoute,
    ResolvingGithubLogin,
    GithubUserNotFound,
    GithubIdentityResolved,
    LoadingBrowserSeed,
    JoiningNetwork,
    QueryingDirectory,
    QueryingRendezvous,
    QueryingKadProviders,
    FeedsFound,
    NoPublicFeeds,
    AuthRequired,
    RequestingSubscriptionGrant,
    DialingPeer,
    RequestingSnapshot,
    SubscribedLive,
    Live,
    OfflineCached,
    DegradedEdgeFallback,
    Failed,
}

impl RemoteRouteState {
    #[must_use]
    pub fn projection_copy(self) -> &'static str {
        match self {
            Self::ParsingRoute => "reading route",
            Self::ResolvingGithubLogin => "resolving github identity",
            Self::GithubUserNotFound => "github user not found",
            Self::GithubIdentityResolved => "github identity found",
            Self::LoadingBrowserSeed => "loading browser seed",
            Self::JoiningNetwork => "joining network",
            Self::QueryingDirectory => "searching mainnet",
            Self::QueryingRendezvous => "querying rendezvous",
            Self::QueryingKadProviders => "querying provider records",
            Self::FeedsFound => "feeds found",
            Self::NoPublicFeeds => "no visible settled story streams",
            Self::AuthRequired => "requesting private feed access",
            Self::RequestingSubscriptionGrant => "requesting subscription grant",
            Self::DialingPeer => "dialing p2p peers",
            Self::RequestingSnapshot => "requesting snapshot",
            Self::SubscribedLive => "connected · waiting for first story",
            Self::Live => "live",
            Self::OfflineCached => "offline cached profile",
            Self::DegradedEdgeFallback => "edge snapshot mode",
            Self::Failed => "unable to connect",
        }
    }

    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::GithubUserNotFound
                | Self::NoPublicFeeds
                | Self::AuthRequired
                | Self::Live
                | Self::OfflineCached
                | Self::DegradedEdgeFallback
                | Self::Failed
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRouteViewModel {
    pub route: RemoteUserRoute,
    pub operating_mode: RemoteFeedMode,
    pub state: RemoteRouteState,
    pub headline: String,
    pub lines: Vec<String>,
    pub ticket: Option<GithubDiscoveryTicket>,
    pub sign_in: Option<BrowserSignInView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSignInView {
    pub provider: String,
    pub label: String,
    pub url: String,
    pub interactive_only: bool,
}

impl BrowserSignInView {
    #[must_use]
    pub fn github(edge_base_url: &str, return_to: &str) -> Self {
        Self {
            provider: "github".to_string(),
            label: "sign in with github".to_string(),
            url: browser_sign_in_url(edge_base_url, return_to),
            interactive_only: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteFeedHeadline {
    pub feed_id: String,
    pub capsule_id: String,
    pub seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub publisher_label: String,
    pub publisher_avatar: Option<String>,
    pub feed_label: String,
    pub headline: String,
    pub deck: String,
    pub lower_third: String,
    pub chips: Vec<String>,
    pub image: Option<HeadlineImage>,
    pub verified: bool,
}

impl RemoteFeedHeadline {
    pub fn from_entry_and_capsule(
        entry: &FeedDirectoryEntry,
        capsule: &Signed<StoryCapsule>,
    ) -> Result<Self, DirectoryError> {
        let view = RemoteHeadlineView::from_entry_and_capsule(entry, capsule)?;
        Ok(Self {
            feed_id: view.feed_id,
            capsule_id: capsule.value.capsule_id.clone(),
            seq: capsule.value.seq,
            created_at: capsule.value.created_at,
            publisher_label: format!("@{}", view.publisher_login),
            publisher_avatar: view.publisher_avatar,
            feed_label: view.feed_label,
            headline: view.headline,
            deck: view.deck,
            lower_third: view.lower_third,
            chips: view.chips,
            image: view.image,
            verified: view.verified,
        })
    }

    #[must_use]
    pub fn timeline_key(&self) -> String {
        format!("{}:{}", self.feed_id, self.capsule_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedTimeline {
    pub selection_syntax: String,
    pub capacity: usize,
    pub items: VecDeque<RemoteFeedHeadline>,
    #[serde(skip)]
    seen: BTreeSet<String>,
}

impl FeedTimeline {
    #[must_use]
    pub fn new(selection_syntax: impl Into<String>, capacity: usize) -> Self {
        Self {
            selection_syntax: selection_syntax.into(),
            capacity: capacity.max(1),
            items: VecDeque::new(),
            seen: BTreeSet::new(),
        }
    }

    pub fn push(&mut self, item: RemoteFeedHeadline) -> bool {
        let key = item.timeline_key();
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key);
        self.items.push_back(item);
        while self.items.len() > self.capacity {
            if let Some(evicted) = self.items.pop_front() {
                self.seen.remove(&evicted.timeline_key());
            }
        }
        true
    }

    #[must_use]
    pub fn newest_first(&self) -> Vec<RemoteFeedHeadline> {
        self.items.iter().rev().cloned().collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineViewModel {
    pub route: RemoteUserRoute,
    pub selection_syntax: String,
    pub wildcard: bool,
    pub timeline: FeedTimeline,
}

impl TimelineViewModel {
    #[must_use]
    pub fn empty(route: RemoteUserRoute, capacity: usize) -> Self {
        let selection_syntax = route.selection.route_syntax(&route.login);
        let wildcard = route.selection.is_wildcard();
        Self {
            route,
            timeline: FeedTimeline::new(selection_syntax.clone(), capacity),
            selection_syntax,
            wildcard,
        }
    }

    pub fn push(&mut self, item: RemoteFeedHeadline) -> bool {
        self.timeline.push(item)
    }
}

impl RemoteRouteViewModel {
    #[must_use]
    pub fn waiting(route: RemoteUserRoute) -> Self {
        Self {
            headline: format!("@{}", route.login),
            lines: vec![
                "resolving github identity".to_string(),
                format!("finding feeds on {}", route.network.network_id()),
                "dialing p2p peers".to_string(),
                "waiting for story capsules".to_string(),
            ],
            route,
            operating_mode: RemoteFeedMode::Discovery,
            state: RemoteRouteState::ResolvingGithubLogin,
            ticket: None,
            sign_in: None,
        }
    }

    #[must_use]
    pub fn with_ticket(mut self, ticket: GithubDiscoveryTicket) -> Self {
        self.state = if ticket.candidate_feeds.is_empty() {
            RemoteRouteState::NoPublicFeeds
        } else {
            RemoteRouteState::FeedsFound
        };
        self.headline = format!("@{}", ticket.profile.login);
        self.lines = vec![
            "github identity found".to_string(),
            format!("{} visible feeds", ticket.candidate_feeds.len()),
            "connected · waiting for first story".to_string(),
        ];
        self.ticket = Some(ticket);
        self.sign_in = None;
        self
    }

    #[must_use]
    pub fn auth_required(mut self, edge_base_url: &str, return_to: &str) -> Self {
        self.state = RemoteRouteState::AuthRequired;
        self.lines = vec![
            "github sign-in required".to_string(),
            "private feeds need a signed browser session".to_string(),
            "projection remains story-only after sign-in".to_string(),
        ];
        self.sign_in = Some(BrowserSignInView::github(edge_base_url, return_to));
        self
    }
}

#[derive(Default)]
struct QueryPairs(Vec<(String, String)>);

impl QueryPairs {
    fn parse(query: &str) -> Self {
        let query = query.trim_start_matches('?');
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

    fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(candidate, _)| candidate == key)
    }
}

fn split_targets(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_core::{AgentEvent, EventKind, SourceKind};
    use agent_feed_directory::GithubPrincipal;
    use agent_feed_identity::GithubUserId;
    use agent_feed_p2p_proto::{FeedVisibility, Signed, StoryCapsule};
    use agent_feed_story::compile_events;
    use time::OffsetDateTime;

    fn public_entry() -> FeedDirectoryEntry {
        let owner = GithubPrincipal {
            github_user_id: GithubUserId::new(123),
            current_login: "mosure".to_string(),
            display_name: Some("mosure".to_string()),
            avatar: Some("/avatar/github/123".to_string()),
            verified_by: "edge".to_string(),
            verified_at: OffsetDateTime::now_utc(),
        };
        FeedDirectoryEntry::new(
            "agent-feed-mainnet",
            "feed-workstation",
            owner,
            "peer-a",
            "workstation",
            FeedVisibility::Public,
            1,
        )
        .sign("peer-a")
        .expect("entry signs")
    }

    fn signed_headline(
        entry: &FeedDirectoryEntry,
        image: Option<HeadlineImage>,
    ) -> Signed<StoryCapsule> {
        let mut event = AgentEvent::new(
            SourceKind::Codex,
            EventKind::TurnComplete,
            "release triage finished",
        );
        event.agent = "codex".to_string();
        event.score_hint = Some(84);
        let story = compile_events([event]).remove(0);
        let mut capsule = StoryCapsule::from_story("feed-workstation", 1, "github:123", &story)
            .expect("capsule builds")
            .with_publisher(entry.publisher_identity())
            .expect("publisher attaches");
        capsule.image = image;
        Signed::sign_capsule(capsule, "peer-a").expect("capsule signs")
    }

    #[test]
    fn waiting_copy_is_stateful_not_spinner_text() {
        let route = RemoteUserRoute::parse("/mosure", Some("all")).expect("route parses");
        let model = RemoteRouteViewModel::waiting(route);
        assert_eq!(model.state, RemoteRouteState::ResolvingGithubLogin);
        assert_eq!(model.operating_mode, RemoteFeedMode::Discovery);
        assert!(model.lines.iter().any(|line| line.contains("github")));
        assert!(model.lines.iter().any(|line| line.contains("p2p")));
    }

    #[test]
    fn feed_mode_query_separates_discovery_from_subscribed() {
        assert_eq!(
            RemoteFeedMode::from_query("feed_mode=discovery"),
            RemoteFeedMode::Discovery
        );
        assert_eq!(
            RemoteFeedMode::from_query("feed_mode=subscribed"),
            RemoteFeedMode::Subscribed
        );
        assert_eq!(
            RemoteFeedMode::from_query("subscriptions=mosure/workstation"),
            RemoteFeedMode::Subscribed
        );
    }

    #[test]
    fn subscribed_operating_mode_keeps_explicit_targets() {
        let mode = RemoteOperatingMode::from_query(
            "mosure/*",
            "feed_mode=subscribed&subscriptions=mosure/workstation,alice/release",
        );

        assert_eq!(mode.mode, RemoteFeedMode::Subscribed);
        assert_eq!(
            mode.subscription_targets,
            vec!["mosure/workstation", "alice/release"]
        );
    }

    #[test]
    fn projection_copy_hides_raw_network_errors() {
        assert_eq!(
            RemoteRouteState::QueryingKadProviders.projection_copy(),
            "querying provider records"
        );
        assert_eq!(
            RemoteRouteState::Failed.projection_copy(),
            "unable to connect"
        );
    }

    #[test]
    fn remote_feed_headline_exposes_publisher_login_and_avatar() {
        let entry = public_entry();
        let signed = signed_headline(&entry, None);

        let headline =
            RemoteFeedHeadline::from_entry_and_capsule(&entry, &signed).expect("headline builds");

        assert_eq!(headline.publisher_label, "@mosure");
        assert_eq!(headline.feed_id, "feed-workstation");
        assert!(headline.capsule_id.starts_with("cap_"));
        assert_eq!(
            headline.publisher_avatar.as_deref(),
            Some("/avatar/github/123")
        );
        assert_eq!(headline.lower_third, "@mosure / workstation");
        assert!(headline.verified);
    }

    #[test]
    fn remote_feed_headline_exposes_optional_headline_image() {
        let entry = public_entry();
        let signed = signed_headline(
            &entry,
            Some(HeadlineImage::new(
                "/assets/headlines/release.webp",
                "abstract release signal",
                "test",
            )),
        );

        let headline =
            RemoteFeedHeadline::from_entry_and_capsule(&entry, &signed).expect("headline builds");

        assert_eq!(
            headline.image.as_ref().map(|image| image.uri.as_str()),
            Some("/assets/headlines/release.webp")
        );
    }

    #[test]
    fn auth_required_view_has_github_sign_in_url() {
        let route = RemoteUserRoute::parse("/mosure", Some("all")).expect("route parses");
        let model = RemoteRouteViewModel::waiting(route)
            .auth_required("https://edge.example", "https://feed.example/mosure?all");

        assert_eq!(model.state, RemoteRouteState::AuthRequired);
        let sign_in = model.sign_in.expect("sign-in view exists");
        assert_eq!(sign_in.provider, "github");
        assert!(sign_in.interactive_only);
        assert!(sign_in.url.starts_with("https://edge.example/auth/github?"));
        assert!(sign_in.url.contains("client=feed-browser"));
    }

    #[test]
    fn timeline_ring_buffer_keeps_recent_unique_headlines() {
        let entry = public_entry();
        let mut timeline = FeedTimeline::new("mosure/workstation", 2);

        let first =
            RemoteFeedHeadline::from_entry_and_capsule(&entry, &signed_headline(&entry, None))
                .expect("first headline");
        let duplicate = first.clone();
        let mut second = first.clone();
        second.capsule_id = "cap_second".to_string();
        second.seq = 2;
        let mut third = first.clone();
        third.capsule_id = "cap_third".to_string();
        third.seq = 3;

        assert!(timeline.push(first));
        assert!(!timeline.push(duplicate));
        assert!(timeline.push(second));
        assert!(timeline.push(third));

        let newest = timeline.newest_first();
        assert_eq!(newest.len(), 2);
        assert_eq!(newest[0].capsule_id, "cap_third");
        assert_eq!(newest[1].capsule_id, "cap_second");
    }

    #[test]
    fn timeline_view_model_preserves_wildcard_selection() {
        let route =
            RemoteUserRoute::parse("/mosure/*", Some("view=timeline")).expect("route parses");
        let model = TimelineViewModel::empty(route, 20);

        assert_eq!(model.selection_syntax, "mosure/*");
        assert!(model.wildcard);
        assert_eq!(model.timeline.capacity, 20);
    }
}
