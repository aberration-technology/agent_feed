use agent_feed_directory::{
    DirectoryError, DirectoryStore, FeedAccessPolicy, GithubDiscoveryTicket, GithubProfileView,
    OrgDiscoveryTicket, OrgRouteFilter, RemoteHeadlineView, RemoteReelFilter, RemoteUserRoute,
    ResolveFeedView, SignedBrowserSeed, ensure_current_compatibility, ensure_network_id,
};
use agent_feed_identity::{GithubLogin, GithubOrgName, GithubTeamSlug};
use agent_feed_identity_github::{
    AllowAllGithubAccess, GithubAccessResolver, GithubResolveError, GithubResolver,
    GithubUserResponse, StaticGithubResolver,
};
use agent_feed_p2p_proto::{
    FeedVisibility, ProtocolCompatibility, PublisherIdentity, Signature, Signed, StoryCapsule,
    github_org_provider_key, github_org_topic, github_provider_key, github_team_provider_key,
    github_team_topic, github_user_topic,
};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use time::{Duration, OffsetDateTime};

const SNAPSHOT_HEADLINE_LIMIT: usize = 48;
const MAX_PUBLISH_CAPSULES: usize = 16;
type HmacSha256 = Hmac<Sha256>;

fn snapshot_headline_ttl() -> Duration {
    Duration::minutes(45)
}

fn snapshot_feed_ttl() -> Duration {
    Duration::minutes(15)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeConfig {
    pub network_id: String,
    pub edge_domain: String,
    pub browser_app_base_url: String,
    pub github_callback_url: String,
    pub bootstrap_peers: Vec<String>,
    pub authority_id: String,
    pub org_policy: OrgDeploymentPolicy,
}

impl EdgeConfig {
    #[must_use]
    pub fn mainnet() -> Self {
        Self {
            network_id: "agent-feed-mainnet".to_string(),
            edge_domain: "https://api.feed.aberration.technology".to_string(),
            browser_app_base_url: "https://feed.aberration.technology".to_string(),
            github_callback_url: "https://api.feed.aberration.technology/callback/github"
                .to_string(),
            bootstrap_peers: vec![
                "/dns4/edge.feed.aberration.technology/tcp/7747".to_string(),
                "/dns4/edge.feed.aberration.technology/udp/7747/quic-v1".to_string(),
                "/dns4/edge.feed.aberration.technology/udp/443/webrtc-direct".to_string(),
            ],
            authority_id: "edge.feed".to_string(),
            org_policy: OrgDeploymentPolicy::from_env(),
        }
    }

    #[must_use]
    pub fn health_path(&self) -> &'static str {
        "/healthz"
    }

    #[must_use]
    pub fn ready_path(&self) -> &'static str {
        "/readyz"
    }

    #[must_use]
    pub fn github_callback_url(&self) -> String {
        let value = self.github_callback_url.trim();
        if value.is_empty() {
            format!(
                "{}/callback/github",
                self.browser_app_base_url.trim_end_matches('/')
            )
        } else {
            value.trim_end_matches('/').to_string()
        }
    }

    #[must_use]
    pub fn github_avatar_url(&self, github_user_id: u64) -> String {
        github_avatar_url(&self.edge_domain, github_user_id)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgDeploymentPolicy {
    pub required_org: Option<GithubOrgName>,
    pub required_teams: Vec<GithubTeamSlug>,
}

impl OrgDeploymentPolicy {
    #[must_use]
    pub fn from_env() -> Self {
        let required_org = env::var("AGENT_FEED_GITHUB_REQUIRED_ORG")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| GithubOrgName::parse(value).ok());
        let required_teams = env::var("AGENT_FEED_GITHUB_REQUIRED_TEAMS")
            .ok()
            .or_else(|| env::var("AGENT_FEED_GITHUB_REQUIRED_TEAM").ok())
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|team| GithubTeamSlug::parse(team).ok())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            required_org,
            required_teams,
        }
    }

    #[must_use]
    pub fn is_restricted(&self) -> bool {
        self.required_org.is_some()
    }
}

impl Default for EdgeConfig {
    fn default() -> Self {
        Self::mainnet()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EdgeError {
    #[error(transparent)]
    Directory(#[from] DirectoryError),
    #[error(transparent)]
    Github(#[from] GithubResolveError),
}

#[derive(Debug, thiserror::Error)]
pub enum EdgeServeError {
    #[error("edge io failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
pub struct EdgeResolver<R = StaticGithubResolver, A = AllowAllGithubAccess> {
    pub config: EdgeConfig,
    pub github: R,
    pub access: A,
    pub directory: DirectoryStore,
}

impl<R: GithubResolver> EdgeResolver<R, AllowAllGithubAccess> {
    #[must_use]
    pub fn new(config: EdgeConfig, github: R, directory: DirectoryStore) -> Self {
        Self {
            config,
            github,
            access: AllowAllGithubAccess,
            directory,
        }
    }
}

impl<R: GithubResolver, A: GithubAccessResolver> EdgeResolver<R, A> {
    #[must_use]
    pub fn new_with_access(
        config: EdgeConfig,
        github: R,
        access: A,
        directory: DirectoryStore,
    ) -> Self {
        Self {
            config,
            github,
            access,
            directory,
        }
    }

    pub fn resolve_github_route(
        &self,
        path: &str,
        query: Option<&str>,
    ) -> Result<GithubDiscoveryTicket, EdgeError> {
        let route = RemoteUserRoute::parse(path, query)?;
        self.resolve_github_user(&route)
    }

    pub fn resolve_github_user(
        &self,
        route: &RemoteUserRoute,
    ) -> Result<GithubDiscoveryTicket, EdgeError> {
        ensure_network_id(&self.config.network_id, &route.network.network_id())?;
        let profile = self.github.resolve_login(&route.login)?;
        self.authorize_profile(profile.id)?;
        let feeds = self
            .directory
            .visible_entries_for_route(profile.id, route)?;
        let seed = SignedBrowserSeed::new(
            self.config.network_id.clone(),
            self.config.edge_domain.clone(),
            self.config.bootstrap_peers.clone(),
            &self.config.authority_id,
        )?;
        let namespace = github_user_topic(&self.config.network_id, profile.id.get());
        let mut rendezvous_namespaces = vec![namespace];
        for feed in &feeds {
            if !rendezvous_namespaces
                .iter()
                .any(|existing| existing == &feed.rendezvous_namespace)
            {
                rendezvous_namespaces.push(feed.rendezvous_namespace.clone());
            }
        }
        let now = OffsetDateTime::now_utc();
        let ticket = GithubDiscoveryTicket {
            network_id: route.network.network_id(),
            compatibility: ProtocolCompatibility::current(),
            requested_login: route.login.clone(),
            resolved_github_id: profile.id,
            profile: GithubProfileView::from(&profile),
            candidate_feeds: feeds,
            bootstrap_peers: self.config.bootstrap_peers.clone(),
            rendezvous_namespaces,
            provider_keys: vec![github_provider_key(
                &self.config.network_id,
                profile.id.get(),
            )],
            browser_seed: seed,
            issued_at: now,
            expires_at: now + Duration::minutes(15),
            signature: agent_feed_p2p_proto::Signature::unsigned(),
        }
        .sign(&self.config.authority_id)?;
        Ok(ticket)
    }

    pub fn resolve_github_org(
        &self,
        org: &GithubOrgName,
        team: Option<&GithubTeamSlug>,
        filter: &OrgRouteFilter,
    ) -> Result<OrgDiscoveryTicket, EdgeError> {
        let feeds = self.directory.visible_entries_for_org(org, team, filter)?;
        let seed = SignedBrowserSeed::new(
            self.config.network_id.clone(),
            self.config.edge_domain.clone(),
            self.config.bootstrap_peers.clone(),
            &self.config.authority_id,
        )?;
        let mut rendezvous_namespaces = vec![match team {
            Some(team) => github_team_topic(&self.config.network_id, org.as_str(), team.as_str()),
            None => github_org_topic(&self.config.network_id, org.as_str()),
        }];
        for feed in &feeds {
            if !rendezvous_namespaces
                .iter()
                .any(|existing| existing == &feed.rendezvous_namespace)
            {
                rendezvous_namespaces.push(feed.rendezvous_namespace.clone());
            }
        }
        let provider_keys = vec![match team {
            Some(team) => {
                github_team_provider_key(&self.config.network_id, org.as_str(), team.as_str())
            }
            None => github_org_provider_key(&self.config.network_id, org.as_str()),
        }];
        let now = OffsetDateTime::now_utc();
        OrgDiscoveryTicket {
            network_id: self.config.network_id.clone(),
            compatibility: ProtocolCompatibility::current(),
            org: org.clone(),
            team: team.cloned(),
            candidate_feeds: feeds,
            bootstrap_peers: self.config.bootstrap_peers.clone(),
            rendezvous_namespaces,
            provider_keys,
            browser_seed: seed,
            issued_at: now,
            expires_at: now + Duration::minutes(15),
            signature: Signature::unsigned(),
        }
        .sign(&self.config.authority_id)
        .map_err(EdgeError::from)
    }

    fn authorize_profile(
        &self,
        github_user_id: agent_feed_identity::GithubUserId,
    ) -> Result<(), EdgeError> {
        let Some(org) = self.config.org_policy.required_org.as_ref() else {
            return Ok(());
        };
        let access = self.access.org_access_for_user(github_user_id, org)?;
        for team in &self.config.org_policy.required_teams {
            if !access.has_team(team) {
                return Err(EdgeError::Github(GithubResolveError::Forbidden(format!(
                    "github team {org}/{team}"
                ))));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveGithubResponse {
    pub state: String,
    pub network_id: String,
    pub compatibility: ProtocolCompatibility,
    pub requested_login: String,
    pub github_user_id: u64,
    pub profile: GithubProfileView,
    pub feeds: Vec<ResolveFeedView>,
    pub headlines: Vec<RemoteHeadlineView>,
    pub browser_seed_url: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub signature: String,
}

impl From<&GithubDiscoveryTicket> for ResolveGithubResponse {
    fn from(ticket: &GithubDiscoveryTicket) -> Self {
        Self::from_ticket(ticket, &EdgeConfig::mainnet())
    }
}

impl ResolveGithubResponse {
    #[must_use]
    pub fn from_ticket(ticket: &GithubDiscoveryTicket, config: &EdgeConfig) -> Self {
        Self::from_ticket_and_headlines(ticket, config, Vec::new())
    }

    #[must_use]
    pub fn from_ticket_and_headlines(
        ticket: &GithubDiscoveryTicket,
        config: &EdgeConfig,
        headlines: Vec<RemoteHeadlineView>,
    ) -> Self {
        let mut feeds: Vec<_> = ticket
            .candidate_feeds
            .iter()
            .map(|feed| resolve_feed_view(feed, config))
            .collect();
        merge_resolve_feed_views(&mut feeds, feed_views_from_headlines(headlines.clone()));
        Self {
            state: "resolved".to_string(),
            network_id: ticket.network_id.clone(),
            compatibility: ticket.compatibility.clone(),
            requested_login: ticket.requested_login.to_string(),
            github_user_id: ticket.resolved_github_id.get(),
            profile: normalized_profile_view(
                &ticket.profile,
                config,
                ticket.resolved_github_id.get(),
            ),
            feeds,
            headlines,
            browser_seed_url: "/browser-seed".to_string(),
            expires_at: ticket.expires_at,
            signature: ticket.signature.digest.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveOrgResponse {
    pub state: String,
    pub network_id: String,
    pub compatibility: ProtocolCompatibility,
    pub org: String,
    pub team: Option<String>,
    pub feeds: Vec<ResolveFeedView>,
    pub headlines: Vec<RemoteHeadlineView>,
    pub browser_seed_url: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub signature: String,
}

impl From<&OrgDiscoveryTicket> for ResolveOrgResponse {
    fn from(ticket: &OrgDiscoveryTicket) -> Self {
        Self::from_ticket(ticket, &EdgeConfig::mainnet())
    }
}

impl ResolveOrgResponse {
    #[must_use]
    pub fn from_ticket(ticket: &OrgDiscoveryTicket, config: &EdgeConfig) -> Self {
        Self::from_ticket_and_headlines(ticket, config, Vec::new())
    }

    #[must_use]
    pub fn from_ticket_and_headlines(
        ticket: &OrgDiscoveryTicket,
        config: &EdgeConfig,
        headlines: Vec<RemoteHeadlineView>,
    ) -> Self {
        let mut feeds: Vec<_> = ticket
            .candidate_feeds
            .iter()
            .map(|feed| resolve_feed_view(feed, config))
            .collect();
        merge_resolve_feed_views(&mut feeds, feed_views_from_headlines(headlines.clone()));
        Self {
            state: "resolved".to_string(),
            network_id: ticket.network_id.clone(),
            compatibility: ticket.compatibility.clone(),
            org: ticket.org.to_string(),
            team: ticket.team.as_ref().map(ToString::to_string),
            feeds,
            headlines,
            browser_seed_url: "/browser-seed".to_string(),
            expires_at: ticket.expires_at,
            signature: ticket.signature.digest.clone(),
        }
    }
}

fn resolve_feed_view(
    feed: &agent_feed_directory::FeedDirectoryEntry,
    config: &EdgeConfig,
) -> ResolveFeedView {
    ResolveFeedView::from_entry(
        feed,
        Some(config.github_avatar_url(feed.owner.github_user_id.get())),
    )
}

fn feed_views_from_headlines(headlines: Vec<RemoteHeadlineView>) -> Vec<ResolveFeedView> {
    let mut feeds = Vec::new();
    for headline in headlines {
        if feeds
            .iter()
            .any(|feed: &ResolveFeedView| feed.feed_id == headline.feed_id)
        {
            continue;
        }
        feeds.push(ResolveFeedView::from_headline(
            headline,
            OffsetDateTime::now_utc(),
        ));
    }
    feeds
}

fn merge_resolve_feed_views(feeds: &mut Vec<ResolveFeedView>, extra: Vec<ResolveFeedView>) {
    for feed in extra {
        if feeds
            .iter()
            .any(|existing| existing.feed_id == feed.feed_id)
        {
            continue;
        }
        feeds.push(feed);
    }
}

fn normalized_profile_view(
    profile: &GithubProfileView,
    config: &EdgeConfig,
    github_user_id: u64,
) -> GithubProfileView {
    GithubProfileView {
        login: profile.login.clone(),
        name: profile.name.clone(),
        avatar: Some(config.github_avatar_url(github_user_id)),
    }
}

fn github_avatar_url(edge_domain: &str, github_user_id: u64) -> String {
    let edge_domain = edge_domain.trim_end_matches('/');
    format!("{edge_domain}/avatar/github/{github_user_id}")
}

fn github_avatar_upstream_url(github_user_id: u64) -> String {
    format!("https://avatars.githubusercontent.com/u/{github_user_id}?v=4&s=192")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeApiPath {
    AuthGithub,
    CallbackGithub,
    ResolveGithub,
    DirectoryGithub,
    DirectoryFeed,
    BrowserSeed,
    AvatarGithub,
    SubscriptionRequest,
    SubscriptionApprove,
    NetworkSnapshot,
    Healthz,
    Readyz,
}

impl EdgeApiPath {
    #[must_use]
    pub fn template(self) -> &'static str {
        match self {
            Self::ResolveGithub => "/resolve/github/{login}",
            Self::AuthGithub => "/auth/github",
            Self::CallbackGithub => "/callback/github",
            Self::DirectoryGithub => "/directory/github/{github_user_id}",
            Self::DirectoryFeed => "/directory/feed/{feed_id}",
            Self::BrowserSeed => "/browser-seed",
            Self::AvatarGithub => "/avatar/github/{github_user_id}",
            Self::SubscriptionRequest => "/subscription/request",
            Self::SubscriptionApprove => "/subscription/approve",
            Self::NetworkSnapshot => "/network/snapshot",
            Self::Healthz => "/healthz",
            Self::Readyz => "/readyz",
        }
    }
}

#[must_use]
pub fn resolve_endpoint(login: &GithubLogin) -> String {
    format!("/resolve/github/{login}")
}

#[derive(Clone, Debug)]
pub struct EdgeServerConfig {
    pub bind: SocketAddr,
    pub edge: EdgeConfig,
    pub fabric: EdgeFabricConfig,
}

impl Default for EdgeServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 7778)),
            edge: EdgeConfig::mainnet(),
            fabric: EdgeFabricConfig::disabled(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeFabricConfig {
    pub enabled: bool,
    pub bind_ip: IpAddr,
    pub tcp_port: u16,
    pub quic_port: u16,
    pub webrtc_direct_port: u16,
}

impl EdgeFabricConfig {
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            tcp_port: 7747,
            quic_port: 7747,
            webrtc_direct_port: 443,
        }
    }

    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::disabled();
        config.enabled = env_bool("AGENT_FEED_EDGE_LISTEN_P2P");
        config.tcp_port = env_u16("AGENT_FEED_P2P_TCP_PORT").unwrap_or(config.tcp_port);
        config.quic_port = env_u16("AGENT_FEED_P2P_QUIC_PORT").unwrap_or(config.quic_port);
        config.webrtc_direct_port =
            env_u16("AGENT_FEED_P2P_WEBRTC_DIRECT_PORT").unwrap_or(config.webrtc_direct_port);
        config
    }
}

#[derive(Clone)]
struct HttpState {
    config: EdgeConfig,
    snapshot: SnapshotStore,
}

#[derive(Clone, Default)]
struct SnapshotStore {
    feeds: Arc<Mutex<VecDeque<ResolveFeedView>>>,
    headlines: Arc<Mutex<VecDeque<RemoteHeadlineView>>>,
    path: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedSnapshot {
    #[serde(default)]
    feeds: Vec<ResolveFeedView>,
    #[serde(default)]
    headlines: Vec<RemoteHeadlineView>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum PersistedSnapshotDisk {
    Current(PersistedSnapshot),
    LegacyHeadlines(Vec<RemoteHeadlineView>),
}

impl SnapshotStore {
    fn from_env() -> Self {
        env::var("AGENT_FEED_EDGE_SNAPSHOT_PATH")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .map(Self::from_path)
            .unwrap_or_default()
    }

    fn from_path(path: PathBuf) -> Self {
        let mut persisted = fs::read_to_string(&path)
            .ok()
            .and_then(|input| serde_json::from_str::<PersistedSnapshotDisk>(&input).ok())
            .map(|disk| match disk {
                PersistedSnapshotDisk::Current(snapshot) => snapshot,
                PersistedSnapshotDisk::LegacyHeadlines(headlines) => PersistedSnapshot {
                    feeds: feed_views_from_headlines(headlines.clone()),
                    headlines,
                },
            })
            .unwrap_or_default();
        prune_persisted_snapshot(&mut persisted);
        Self {
            feeds: Arc::new(Mutex::new(VecDeque::from(persisted.feeds))),
            headlines: Arc::new(Mutex::new(VecDeque::from(persisted.headlines))),
            path: Some(path),
        }
    }

    fn upsert_feed(&self, feed: ResolveFeedView) {
        let feeds = {
            let Ok(mut feeds) = self.feeds.lock() else {
                tracing::warn!("network snapshot feed store lock poisoned");
                return;
            };
            let now = OffsetDateTime::now_utc();
            feeds.retain(|existing| feed_is_within_retention(existing, now));
            upsert_feed_view(&mut feeds, feed);
            while feeds.len() > SNAPSHOT_HEADLINE_LIMIT {
                feeds.pop_front();
            }
            feeds.iter().cloned().collect()
        };
        let headlines = self.headlines();
        self.persist_snapshot(feeds, headlines);
    }

    fn push(&self, headline: RemoteHeadlineView) -> bool {
        let now = OffsetDateTime::now_utc();
        if !headline_is_within_retention(&headline, now) {
            tracing::warn!(
                feed_id = %headline.feed_id,
                publisher = %headline.publisher_login,
                created_at = ?headline.created_at,
                "stale network headline ignored"
            );
            return false;
        }
        if let Some(feed) = feed_views_from_headlines(vec![headline.clone()])
            .into_iter()
            .next()
        {
            self.upsert_feed(feed);
        }
        let Ok(mut headlines) = self.headlines.lock() else {
            tracing::warn!("network snapshot headline store lock poisoned");
            return false;
        };
        headlines.retain(|existing| headline_is_within_retention(existing, now));
        if headlines.iter().any(|existing| {
            existing.feed_id == headline.feed_id
                && existing.headline == headline.headline
                && existing.deck == headline.deck
        }) {
            return false;
        }
        headlines.push_back(headline);
        while headlines.len() > SNAPSHOT_HEADLINE_LIMIT {
            headlines.pop_front();
        }
        let feeds = self
            .feeds
            .lock()
            .map(|feeds| feeds.iter().cloned().collect())
            .unwrap_or_default();
        self.persist_snapshot(feeds, headlines.iter().cloned().collect());
        true
    }

    fn headlines(&self) -> Vec<RemoteHeadlineView> {
        let now = OffsetDateTime::now_utc();
        self.headlines
            .lock()
            .map(|headlines| {
                headlines
                    .iter()
                    .filter(|headline| public_headline_is_visible(headline))
                    .filter(|headline| headline_is_within_retention(headline, now))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn feeds(&self) -> Vec<ResolveFeedView> {
        let now = OffsetDateTime::now_utc();
        let mut feeds = self
            .feeds
            .lock()
            .map(|feeds| {
                feeds
                    .iter()
                    .filter(|feed| public_feed_is_visible(feed))
                    .filter(|feed| feed_is_within_retention(feed, now))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        merge_resolve_feed_views(&mut feeds, feed_views_from_headlines(self.headlines()));
        feeds
    }

    fn headlines_for_org(
        &self,
        org: &GithubOrgName,
        team: Option<&GithubTeamSlug>,
        filter: &OrgRouteFilter,
    ) -> Vec<RemoteHeadlineView> {
        let now = OffsetDateTime::now_utc();
        self.headlines
            .lock()
            .map(|headlines| {
                headlines
                    .iter()
                    .filter(|headline| headline_is_within_retention(headline, now))
                    .filter(|headline| headline_matches_org_route(headline, org, team, filter))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn feeds_for_org(
        &self,
        org: &GithubOrgName,
        team: Option<&GithubTeamSlug>,
        filter: &OrgRouteFilter,
    ) -> Vec<ResolveFeedView> {
        let now = OffsetDateTime::now_utc();
        let mut feeds = self
            .feeds
            .lock()
            .map(|feeds| {
                feeds
                    .iter()
                    .filter(|feed| feed_is_within_retention(feed, now))
                    .filter(|feed| feed_matches_org_route(feed, org, team, filter))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        merge_resolve_feed_views(
            &mut feeds,
            feed_views_from_headlines(self.headlines_for_org(org, team, filter)),
        );
        feeds
    }

    fn persist_snapshot(&self, feeds: Vec<ResolveFeedView>, headlines: Vec<RemoteHeadlineView>) {
        let Some(path) = &self.path else {
            return;
        };
        let now = OffsetDateTime::now_utc();
        let feeds = feeds
            .into_iter()
            .filter(|feed| feed_is_within_retention(feed, now))
            .collect();
        let headlines = headlines
            .into_iter()
            .filter(|headline| headline_is_within_retention(headline, now))
            .collect();
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            tracing::warn!(path = %parent.display(), error = %err, "failed to create edge snapshot dir");
            return;
        }
        let tmp = path.with_extension("json.tmp");
        let payload = PersistedSnapshot { feeds, headlines };
        match serde_json::to_vec_pretty(&payload)
            .map_err(std::io::Error::other)
            .and_then(|bytes| fs::write(&tmp, bytes))
            .and_then(|()| fs::rename(&tmp, path))
        {
            Ok(()) => {}
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                tracing::warn!(path = %path.display(), error = %err, "failed to persist edge snapshot");
            }
        }
    }
}

fn prune_persisted_snapshot(snapshot: &mut PersistedSnapshot) {
    let now = OffsetDateTime::now_utc();
    snapshot
        .feeds
        .retain(|feed| feed_is_within_retention(feed, now));
    snapshot
        .headlines
        .retain(|headline| headline_is_within_retention(headline, now));
}

fn headline_is_within_retention(headline: &RemoteHeadlineView, now: OffsetDateTime) -> bool {
    headline
        .created_at
        .map(|created_at| now - created_at <= snapshot_headline_ttl())
        .unwrap_or(true)
}

fn feed_is_within_retention(feed: &ResolveFeedView, now: OffsetDateTime) -> bool {
    now - feed.last_seen_at <= snapshot_feed_ttl()
}

fn upsert_feed_view(feeds: &mut VecDeque<ResolveFeedView>, feed: ResolveFeedView) {
    if let Some(existing) = feeds
        .iter_mut()
        .find(|existing| existing.feed_id == feed.feed_id)
    {
        *existing = feed;
        return;
    }
    feeds.push_back(feed);
}

pub async fn serve_http(config: EdgeServerConfig) -> Result<(), EdgeServeError> {
    if config.fabric.enabled {
        spawn_fabric_listeners(config.edge.clone(), config.fabric.clone()).await?;
    }
    let state = Arc::new(HttpState {
        config: config.edge,
        snapshot: SnapshotStore::from_env(),
    });
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/auth/github", get(auth_github))
        .route("/callback/github", get(callback_github))
        .route("/resolve/github/{login}", get(resolve_github))
        .route("/resolve/github-org/{org}", get(resolve_github_org))
        .route(
            "/resolve/github-org/{org}/teams/{team}",
            get(resolve_github_team),
        )
        .route("/avatar/github/{github_user_id}", get(avatar_github))
        .route("/browser-seed", get(browser_seed))
        .route("/network/snapshot", get(network_snapshot))
        .route("/network/publish", post(network_publish))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(bind = %config.bind, "feed edge serving");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn spawn_fabric_listeners(
    edge: EdgeConfig,
    fabric: EdgeFabricConfig,
) -> Result<(), EdgeServeError> {
    let tcp_addr = SocketAddr::new(fabric.bind_ip, fabric.tcp_port);
    let tcp_listener = tokio::net::TcpListener::bind(tcp_addr).await?;
    let tcp_edge = edge.clone();
    tokio::spawn(async move {
        loop {
            match tcp_listener.accept().await {
                Ok((stream, peer)) => {
                    let edge = tcp_edge.clone();
                    tokio::spawn(async move {
                        let payload = fabric_probe_payload(&edge, "tcp");
                        match stream.writable().await {
                            Ok(()) => {
                                if let Err(error) = stream.try_write(payload.as_bytes()) {
                                    tracing::warn!(%peer, %error, "feed fabric tcp probe write failed");
                                }
                            }
                            Err(error) => {
                                tracing::warn!(%peer, %error, "feed fabric tcp probe not writable");
                            }
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "feed fabric tcp accept failed");
                }
            }
        }
    });
    tracing::info!(%tcp_addr, "feed fabric tcp listener serving");

    spawn_udp_probe_listener(
        SocketAddr::new(fabric.bind_ip, fabric.quic_port),
        edge.clone(),
        "quic",
    )
    .await?;
    if fabric.webrtc_direct_port != fabric.quic_port {
        spawn_udp_probe_listener(
            SocketAddr::new(fabric.bind_ip, fabric.webrtc_direct_port),
            edge,
            "webrtc-direct",
        )
        .await?;
    }
    Ok(())
}

async fn spawn_udp_probe_listener(
    addr: SocketAddr,
    edge: EdgeConfig,
    transport: &'static str,
) -> Result<(), EdgeServeError> {
    let socket = tokio::net::UdpSocket::bind(addr).await?;
    tokio::spawn(async move {
        let mut buf = [0_u8; 2048];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((_len, peer)) => {
                    let payload = fabric_probe_payload(&edge, transport);
                    if let Err(error) = socket.send_to(payload.as_bytes(), peer).await {
                        tracing::warn!(%peer, %error, transport, "feed fabric udp probe failed");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, transport, "feed fabric udp receive failed");
                }
            }
        }
    });
    tracing::info!(%addr, transport, "feed fabric udp listener serving");
    Ok(())
}

fn fabric_probe_payload(edge: &EdgeConfig, transport: &str) -> String {
    edge_status_payload(Some(edge), Some(transport)).to_string() + "\n"
}

fn env_bool(name: &str) -> bool {
    env::var(name)
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
        .unwrap_or(false)
}

fn env_u16(name: &str) -> Option<u16> {
    env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|port| *port > 0)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz() -> Json<serde_json::Value> {
    Json(edge_status_payload(None, None))
}

fn edge_status_payload(edge: Option<&EdgeConfig>, transport: Option<&str>) -> serde_json::Value {
    let compatibility = ProtocolCompatibility::current();
    let mut payload = serde_json::json!({
        "state": "ready",
        "product": compatibility.product,
        "protocol": agent_feed_p2p_proto::AGENT_FEED_EDGE_PROTOCOL,
        "protocol_version": compatibility.protocol_version,
        "model_version": compatibility.model_version,
        "min_model_version": compatibility.min_model_version,
        "release_version": compatibility.release_version,
        "bootstrap_topology": "single_bootstrap",
        "data_plane": "edge_snapshot_fallback",
    });
    let Some(object) = payload.as_object_mut() else {
        return payload;
    };
    if let Some(edge) = edge {
        object.insert(
            "network_id".to_string(),
            serde_json::Value::String(edge.network_id.clone()),
        );
        object.insert(
            "edge".to_string(),
            serde_json::Value::String(edge.edge_domain.clone()),
        );
    }
    if let Some(transport) = transport {
        object.insert(
            "transport".to_string(),
            serde_json::Value::String(transport.to_string()),
        );
    }
    payload
}

async fn auth_github(
    State(state): State<Arc<HttpState>>,
    Query(query): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    let Some(client_id) = github_client_id() else {
        return edge_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "github auth is not configured",
        );
    };
    let client = query
        .get("client")
        .cloned()
        .unwrap_or_else(|| "feed-browser".to_string());
    if !matches!(
        client.as_str(),
        "feed-browser" | "feed-cli" | "agent-feed-browser" | "agent-feed-cli"
    ) {
        return edge_error(StatusCode::BAD_REQUEST, "unsupported github auth client");
    }
    let return_to = query
        .get("return_to")
        .cloned()
        .unwrap_or_else(|| state.config.browser_app_base_url.clone());
    let redirect_uri = query.get("redirect_uri").cloned();
    if client.ends_with("-cli") {
        let Some(uri) = redirect_uri.as_deref() else {
            return edge_error(StatusCode::BAD_REQUEST, "cli redirect_uri is required");
        };
        if !is_loopback_callback(uri) {
            return edge_error(StatusCode::BAD_REQUEST, "cli redirect_uri must be loopback");
        }
    } else if !is_allowed_browser_return(&state.config, &return_to) {
        return edge_error(StatusCode::BAD_REQUEST, "browser return_to is not allowed");
    }

    let payload = GithubOauthState {
        client,
        state: query.get("state").cloned().unwrap_or_default(),
        redirect_uri,
        return_to,
        github_org: query
            .get("org")
            .cloned()
            .filter(|value| !value.trim().is_empty()),
    };
    let Ok(encoded_state) = encode_oauth_state(&payload) else {
        return edge_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "github auth state failed",
        );
    };
    let callback_url = state.config.github_callback_url();
    let default_scope = if payload.github_org.is_some() {
        "read:user read:org".to_string()
    } else {
        default_github_scope(&state.config.org_policy)
    };
    let scope = query.get("scope").cloned().unwrap_or(default_scope);
    let authorize_url = format!(
        "https://github.com/login/oauth/authorize?{}",
        encode_query(&[
            ("client_id", client_id.as_str()),
            ("redirect_uri", callback_url.as_str()),
            ("state", encoded_state.as_str()),
            ("scope", scope.as_str()),
        ])
    );
    Redirect::temporary(&authorize_url).into_response()
}

async fn callback_github(
    State(state): State<Arc<HttpState>>,
    Query(query): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    let Some(client_id) = github_client_id() else {
        return edge_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "github auth is not configured",
        );
    };
    let Some(client_secret) = github_client_secret() else {
        return edge_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "github auth secret is not configured",
        );
    };
    let Some(code) = query.get("code").filter(|value| !value.is_empty()) else {
        return edge_error(StatusCode::BAD_REQUEST, "github callback missing code");
    };
    let Some(raw_state) = query.get("state").filter(|value| !value.is_empty()) else {
        return edge_error(StatusCode::BAD_REQUEST, "github callback missing state");
    };
    let payload = match decode_oauth_state(raw_state) {
        Ok(payload) => payload,
        Err(message) => return edge_error(StatusCode::BAD_REQUEST, message),
    };
    if payload.client.ends_with("-cli") {
        let Some(uri) = payload.redirect_uri.as_deref() else {
            return edge_error(StatusCode::BAD_REQUEST, "cli redirect_uri is required");
        };
        if !is_loopback_callback(uri) {
            return edge_error(StatusCode::BAD_REQUEST, "cli redirect_uri must be loopback");
        }
    } else if !is_allowed_browser_return(&state.config, &payload.return_to) {
        return edge_error(StatusCode::BAD_REQUEST, "browser return_to is not allowed");
    }

    let callback_url = state.config.github_callback_url();
    let token = match exchange_github_code(&client_id, &client_secret, code, &callback_url) {
        Ok(token) => token,
        Err(message) => return edge_error(StatusCode::BAD_GATEWAY, message),
    };
    let profile = match fetch_github_profile(&token.access_token) {
        Ok(profile) => profile,
        Err(message) => return edge_error(StatusCode::BAD_GATEWAY, message),
    };
    if let Err(message) =
        authorize_github_token_policy(&token.access_token, &profile, &state.config.org_policy)
    {
        return edge_error(StatusCode::FORBIDDEN, message);
    }
    let mut authorized_orgs = Vec::new();
    let mut authorized_teams = Vec::new();
    if let Some(org) = payload.github_org.as_deref() {
        let org = match GithubOrgName::parse(org) {
            Ok(org) => org,
            Err(err) => return edge_error(StatusCode::BAD_REQUEST, err.to_string()),
        };
        if !token.scopes.iter().any(|scope| scope == "read:org") {
            return edge_error(
                StatusCode::FORBIDDEN,
                "github org feeds require read:org scope",
            );
        }
        if let Err(message) =
            fetch_github_org_membership(&token.access_token, &org, profile.login.as_str())
        {
            return edge_error(StatusCode::FORBIDDEN, message);
        }
        match fetch_github_user_teams(&token.access_token) {
            Ok(teams) => {
                authorized_teams.extend(
                    teams
                        .into_iter()
                        .filter(|team| team.org == org)
                        .map(|team| format!("{}/{}", team.org.as_str(), team.slug.as_str())),
                );
            }
            Err(err) => {
                tracing::warn!(org = %org, error = %err, "github team claim fetch failed after org membership check");
            }
        }
        authorized_orgs.push(org.as_str().to_ascii_lowercase());
    }
    let expires_at = OffsetDateTime::now_utc() + Duration::days(7);
    let session = match issue_session_token_for_auth(
        profile.id.get(),
        expires_at,
        token.scopes.clone(),
        authorized_orgs.clone(),
        authorized_teams.clone(),
    ) {
        Some(session) => session,
        None => {
            return edge_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "github session signing is not configured",
            );
        }
    };
    let final_url = if payload.client.ends_with("-cli") {
        payload.redirect_uri.unwrap_or_default()
    } else {
        format!(
            "{}/callback/github",
            origin_of_url(&payload.return_to)
                .unwrap_or_else(|| state.config.browser_app_base_url.clone())
        )
    };
    let github_user_id = profile.id.get().to_string();
    let avatar_url = state.config.github_avatar_url(profile.id.get());
    let expires_at = expires_at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let scopes = token.scopes.join(" ");
    let github_orgs = authorized_orgs.join(",");
    let callback_query = encode_query(&[
        ("state", payload.state.as_str()),
        ("github_user_id", github_user_id.as_str()),
        ("login", profile.login.as_str()),
        ("name", profile.name.as_deref().unwrap_or("")),
        ("avatar_url", avatar_url.as_str()),
        ("session", session.as_str()),
        ("scopes", scopes.as_str()),
        ("github_orgs", github_orgs.as_str()),
        ("expires_at", expires_at.as_str()),
        ("return_to", payload.return_to.as_str()),
    ]);
    let separator = if payload.client.ends_with("-cli") {
        "?"
    } else {
        "#"
    };
    let redirect_url = format!("{final_url}{separator}{callback_query}");
    Redirect::temporary(&redirect_url).into_response()
}

async fn resolve_github(
    State(state): State<Arc<HttpState>>,
    Path(login): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    let query = query
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let route_query = (!query.is_empty()).then_some(query.as_str());
    let route = match RemoteUserRoute::parse(&format!("/{login}"), route_query) {
        Ok(route) => route,
        Err(err) => return edge_error(StatusCode::BAD_REQUEST, err.to_string()),
    };
    let resolver = EdgeResolver::new(
        state.config.clone(),
        CurlGithubResolver,
        DirectoryStore::new(),
    );
    match resolver.resolve_github_user(&route) {
        Ok(ticket) => {
            let feeds = state
                .snapshot
                .feeds()
                .into_iter()
                .filter(|feed| {
                    feed_matches_github_route(
                        feed,
                        ticket.resolved_github_id.get(),
                        ticket.profile.login.as_str(),
                        &route,
                    )
                })
                .collect::<Vec<_>>();
            let headlines = state
                .snapshot
                .headlines()
                .into_iter()
                .filter(|headline| {
                    headline_matches_github_route(
                        headline,
                        ticket.resolved_github_id.get(),
                        ticket.profile.login.as_str(),
                        &route,
                    )
                })
                .collect();
            let mut response =
                ResolveGithubResponse::from_ticket_and_headlines(&ticket, &state.config, headlines);
            merge_resolve_feed_views(&mut response.feeds, feeds);
            ([(header::CACHE_CONTROL, "no-store")], Json(response)).into_response()
        }
        Err(EdgeError::Github(GithubResolveError::NotFound(_))) => {
            if let Some(response) =
                resolve_github_from_snapshot(&state.config, &state.snapshot, &route)
            {
                return ([(header::CACHE_CONTROL, "no-store")], Json(response)).into_response();
            }
            edge_error(StatusCode::NOT_FOUND, "github user not found")
        }
        Err(EdgeError::Github(GithubResolveError::RateLimited)) => edge_error(
            StatusCode::TOO_MANY_REQUESTS,
            "github resolver rate limited",
        ),
        Err(
            err @ EdgeError::Directory(
                DirectoryError::NetworkMismatch { .. } | DirectoryError::IncompatibleProtocol(_),
            ),
        ) => edge_error(StatusCode::UPGRADE_REQUIRED, err.to_string()),
        Err(err) => edge_error(StatusCode::BAD_GATEWAY, err.to_string()),
    }
}

async fn resolve_github_org(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Path(org): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    resolve_org_like(state, headers, org, None, query)
}

async fn resolve_github_team(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Path((org, team)): Path<(String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    resolve_org_like(state, headers, org, Some(team), query)
}

fn resolve_org_like(
    state: Arc<HttpState>,
    headers: HeaderMap,
    org: String,
    team: Option<String>,
    query: BTreeMap<String, String>,
) -> axum::response::Response {
    let org = match GithubOrgName::parse(&org) {
        Ok(org) => org,
        Err(err) => return edge_error(StatusCode::BAD_REQUEST, err.to_string()),
    };
    let team = match team {
        Some(team) => match GithubTeamSlug::parse(team) {
            Ok(team) => Some(team),
            Err(err) => return edge_error(StatusCode::BAD_REQUEST, err.to_string()),
        },
        None => None,
    };
    let Some(bearer) = verify_bearer_session(&headers) else {
        return edge_error(
            StatusCode::UNAUTHORIZED,
            "github session required for private org feeds",
        );
    };
    if !bearer.session.has_scope("read:org") {
        return edge_error(
            StatusCode::FORBIDDEN,
            "github org feeds require read:org scope; sign in again for this org",
        );
    }
    if !bearer.session.permits_org(&org) {
        return edge_error(
            StatusCode::FORBIDDEN,
            format!("github org membership required: {org}"),
        );
    }
    if let Some(team) = team.as_ref()
        && !bearer.session.permits_team(&org, team)
    {
        return edge_error(
            StatusCode::FORBIDDEN,
            format!("github team membership required: {org}/{team}"),
        );
    }
    let query = query
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let filter = match OrgRouteFilter::from_query((!query.is_empty()).then_some(query.as_str())) {
        Ok(filter) => filter,
        Err(err) => return edge_error(StatusCode::BAD_REQUEST, err.to_string()),
    };
    let resolver = EdgeResolver::new(
        state.config.clone(),
        CurlGithubResolver,
        DirectoryStore::new(),
    );
    match resolver.resolve_github_org(&org, team.as_ref(), &filter) {
        Ok(ticket) => {
            let mut response = ResolveOrgResponse::from_ticket_and_headlines(
                &ticket,
                &state.config,
                state
                    .snapshot
                    .headlines_for_org(&org, team.as_ref(), &filter),
            );
            merge_resolve_feed_views(
                &mut response.feeds,
                state.snapshot.feeds_for_org(&org, team.as_ref(), &filter),
            );
            Json(response).into_response()
        }
        Err(err) => edge_error(StatusCode::BAD_GATEWAY, err.to_string()),
    }
}

async fn avatar_github(Path(github_user_id): Path<u64>) -> impl IntoResponse {
    if github_user_id == 0 {
        return edge_error(StatusCode::BAD_REQUEST, "invalid github user id");
    }
    (
        [(
            header::CACHE_CONTROL,
            "public, max-age=86400, stale-while-revalidate=604800",
        )],
        Redirect::temporary(&github_avatar_upstream_url(github_user_id)),
    )
        .into_response()
}

async fn browser_seed(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    match SignedBrowserSeed::new(
        state.config.network_id.clone(),
        state.config.edge_domain.clone(),
        state.config.bootstrap_peers.clone(),
        &state.config.authority_id,
    ) {
        Ok(seed) => Json(seed).into_response(),
        Err(err) => edge_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn network_snapshot(
    State(state): State<Arc<HttpState>>,
    Query(query): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    let requested = requested_network_id(query.get("network").map(String::as_str), &state.config);
    if let Err(err) = ensure_network_id(&state.config.network_id, &requested) {
        return edge_error(StatusCode::UPGRADE_REQUIRED, err.to_string());
    }
    let query = query
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let filter = match RemoteReelFilter::from_query((!query.is_empty()).then_some(query.as_str())) {
        Ok(filter) => filter,
        Err(err) => return edge_error(StatusCode::BAD_REQUEST, err.to_string()),
    };
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(network_snapshot_value_filtered(
            &state.config,
            &state.snapshot,
            Some(&filter),
        )),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct NetworkPublishRequest {
    #[serde(default)]
    network_id: Option<String>,
    #[serde(default)]
    compatibility: Option<ProtocolCompatibility>,
    #[serde(default)]
    feed_name: Option<String>,
    #[serde(default)]
    feed_id: Option<String>,
    #[serde(default)]
    publisher: Option<PublisherIdentity>,
    #[serde(default)]
    visibility: Option<FeedVisibility>,
    #[serde(default)]
    github_org: Option<String>,
    #[serde(default)]
    github_team: Option<String>,
    #[serde(default)]
    capsules: Vec<Signed<StoryCapsule>>,
}

#[derive(Debug, Serialize)]
struct NetworkPublishResponse {
    state: &'static str,
    compatibility: ProtocolCompatibility,
    accepted: usize,
    feeds: usize,
    headlines: usize,
}

async fn network_publish(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(request): Json<NetworkPublishRequest>,
) -> impl IntoResponse {
    let Some(bearer) = verify_bearer_session(&headers) else {
        return edge_error(StatusCode::UNAUTHORIZED, "github session required");
    };
    network_publish_verified(state, bearer, request).await
}

async fn network_publish_verified(
    state: Arc<HttpState>,
    bearer: VerifiedBearer,
    request: NetworkPublishRequest,
) -> axum::response::Response {
    let session = bearer.session;
    let Some(network_id) = request.network_id.as_deref() else {
        return edge_error(
            StatusCode::BAD_REQUEST,
            "network_id is required; update your peer to the latest version",
        );
    };
    if let Err(err) = ensure_network_id(&state.config.network_id, network_id) {
        return edge_error(StatusCode::BAD_REQUEST, err.to_string());
    }
    let Some(compatibility) = request.compatibility.as_ref() else {
        return edge_error(
            StatusCode::UPGRADE_REQUIRED,
            "compatibility metadata is required; update your peer to the latest version",
        );
    };
    if let Err(err) = ensure_current_compatibility(compatibility) {
        return edge_error(StatusCode::UPGRADE_REQUIRED, err.to_string());
    }
    let feed_name = request
        .feed_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("workstation");
    let feed_id = request
        .feed_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("local:{feed_name}"));
    let (visibility, access) = match publish_access_policy(&request, &session) {
        Ok(access) => access,
        Err(response) => return response,
    };
    if let Some(publisher) = request.publisher.as_ref() {
        if publisher.github_user_id != Some(session.github_user_id) || !publisher.verified {
            return edge_error(StatusCode::FORBIDDEN, "publisher identity mismatch");
        }
        state.snapshot.upsert_feed(feed_view_from_publish(
            &feed_id,
            feed_name,
            compatibility,
            visibility,
            access.clone(),
            publisher,
        ));
    }
    if request.capsules.is_empty() {
        return Json(NetworkPublishResponse {
            state: "accepted",
            compatibility: ProtocolCompatibility::current(),
            accepted: 0,
            feeds: state.snapshot.feeds().len(),
            headlines: state.snapshot.headlines().len(),
        })
        .into_response();
    }
    if request.capsules.len() > MAX_PUBLISH_CAPSULES {
        return edge_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("publish batch exceeds {MAX_PUBLISH_CAPSULES} capsules"),
        );
    }
    let mut accepted = 0usize;
    for capsule in request.capsules {
        if let Err(err) = ensure_current_compatibility(&capsule.value.compatibility) {
            return edge_error(StatusCode::UPGRADE_REQUIRED, err.to_string());
        }
        if !capsule
            .verify_capsule_with_secret(&bearer.token)
            .unwrap_or(false)
        {
            return edge_error(StatusCode::BAD_REQUEST, "invalid capsule signature");
        }
        if capsule.value.privacy_class != agent_feed_core::PrivacyClass::Redacted {
            return edge_error(StatusCode::BAD_REQUEST, "capsule must be redacted");
        }
        let Some(publisher) = capsule.value.publisher.clone() else {
            return edge_error(StatusCode::BAD_REQUEST, "verified publisher is required");
        };
        if publisher.github_user_id != Some(session.github_user_id) || !publisher.verified {
            return edge_error(StatusCode::FORBIDDEN, "publisher identity mismatch");
        }
        let publisher_login = publisher
            .github_login
            .clone()
            .unwrap_or_else(|| format!("github:{}", session.github_user_id));
        let view = RemoteHeadlineView {
            feed_id: capsule.value.feed_id.clone(),
            feed_label: feed_name.to_string(),
            compatibility: capsule.value.compatibility.clone(),
            visibility: visibility.as_str().to_string(),
            access: access.clone(),
            created_at: Some(capsule.value.created_at),
            publisher_github_user_id: publisher.github_user_id,
            publisher_login,
            publisher_display_name: publisher.display_name.clone(),
            publisher_avatar: publisher.avatar.clone(),
            verified: true,
            headline: capsule.value.headline.clone(),
            deck: capsule.value.deck.clone(),
            lower_third: format!("{} / {}", publisher.display_label(), feed_name),
            chips: capsule.value.chips.clone(),
            score: capsule.value.score,
            image: capsule.value.image.clone(),
        };
        if state.snapshot.push(view) {
            accepted += 1;
        }
    }
    tracing::info!(
        github_user_id = session.github_user_id,
        feed_name,
        accepted,
        "network story capsules accepted"
    );
    Json(NetworkPublishResponse {
        state: "accepted",
        compatibility: ProtocolCompatibility::current(),
        accepted,
        feeds: state.snapshot.feeds().len(),
        headlines: state.snapshot.headlines().len(),
    })
    .into_response()
}

fn publish_access_policy(
    request: &NetworkPublishRequest,
    session: &VerifiedSession,
) -> Result<(FeedVisibility, FeedAccessPolicy), axum::response::Response> {
    let visibility = request.visibility.unwrap_or(FeedVisibility::Public);
    match visibility {
        FeedVisibility::Public => Ok((visibility, FeedAccessPolicy::public())),
        FeedVisibility::GithubOrg => {
            if !session.has_scope("read:org") {
                return Err(edge_error(
                    StatusCode::FORBIDDEN,
                    "github org publishing requires read:org scope; run `agent-feed auth github --github-org <org>`",
                ));
            }
            let Some(org) = request
                .github_org
                .as_deref()
                .map(str::trim)
                .filter(|org| !org.is_empty())
            else {
                return Err(edge_error(
                    StatusCode::BAD_REQUEST,
                    "github_org is required for github_org visibility",
                ));
            };
            let org = match GithubOrgName::parse(org) {
                Ok(org) => org,
                Err(err) => return Err(edge_error(StatusCode::BAD_REQUEST, err.to_string())),
            };
            if !session.permits_org(&org) {
                return Err(edge_error(
                    StatusCode::FORBIDDEN,
                    format!("github org membership required: {org}; sign in again for this org"),
                ));
            }
            Ok((visibility, FeedAccessPolicy::github_org(org)))
        }
        FeedVisibility::GithubTeam => {
            if !session.has_scope("read:org") {
                return Err(edge_error(
                    StatusCode::FORBIDDEN,
                    "github team publishing requires read:org scope; run `agent-feed auth github --github-org <org>`",
                ));
            }
            let Some(org) = request
                .github_org
                .as_deref()
                .map(str::trim)
                .filter(|org| !org.is_empty())
            else {
                return Err(edge_error(
                    StatusCode::BAD_REQUEST,
                    "github_org is required for github_team visibility",
                ));
            };
            let Some(team) = request
                .github_team
                .as_deref()
                .map(str::trim)
                .filter(|team| !team.is_empty())
            else {
                return Err(edge_error(
                    StatusCode::BAD_REQUEST,
                    "github_team is required for github_team visibility",
                ));
            };
            let org = match GithubOrgName::parse(org) {
                Ok(org) => org,
                Err(err) => return Err(edge_error(StatusCode::BAD_REQUEST, err.to_string())),
            };
            let team = match GithubTeamSlug::parse(team) {
                Ok(team) => team,
                Err(err) => return Err(edge_error(StatusCode::BAD_REQUEST, err.to_string())),
            };
            if !session.permits_team(&org, &team) {
                return Err(edge_error(
                    StatusCode::FORBIDDEN,
                    format!(
                        "github team membership required: {org}/{team}; sign in again for this org"
                    ),
                ));
            }
            Ok((visibility, FeedAccessPolicy::github_team(org, team)))
        }
        _ => Err(edge_error(
            StatusCode::BAD_REQUEST,
            "edge publish supports public, github_org, and github_team visibility",
        )),
    }
}

fn feed_view_from_publish(
    feed_id: &str,
    feed_name: &str,
    compatibility: &ProtocolCompatibility,
    visibility: FeedVisibility,
    access: FeedAccessPolicy,
    publisher: &PublisherIdentity,
) -> ResolveFeedView {
    ResolveFeedView::from_publisher(
        feed_id,
        feed_name,
        compatibility.clone(),
        visibility,
        access,
        publisher,
        OffsetDateTime::now_utc(),
    )
}

fn headline_matches_github_route(
    headline: &RemoteHeadlineView,
    github_user_id: u64,
    github_login: &str,
    route: &RemoteUserRoute,
) -> bool {
    let identity_matches = headline.publisher_github_user_id == Some(github_user_id)
        || (headline.publisher_github_user_id.is_none()
            && headline.publisher_login.eq_ignore_ascii_case(github_login));
    identity_matches
        && public_headline_is_visible(headline)
        && route.stream_filter.permits_label(&headline.feed_label)
        && headline_matches_reel_filter(headline, &route.reel_filter)
}

fn feed_matches_github_route(
    feed: &ResolveFeedView,
    github_user_id: u64,
    github_login: &str,
    route: &RemoteUserRoute,
) -> bool {
    let identity_matches = feed.publisher_github_user_id == Some(github_user_id)
        || feed.publisher_login.eq_ignore_ascii_case(github_login);
    identity_matches
        && public_feed_is_visible(feed)
        && route.stream_filter.permits_label(&feed.label)
}

fn public_feed_is_visible(feed: &ResolveFeedView) -> bool {
    feed.visibility == FeedVisibility::Public.as_str() && !feed.feed_id.starts_with("local:")
}

fn public_headline_is_visible(headline: &RemoteHeadlineView) -> bool {
    headline.visibility == FeedVisibility::Public.as_str()
        && !headline.feed_id.starts_with("local:")
        && !remote_copy_has_public_quality_issue(&format!(
            "{} {}",
            headline.headline, headline.deck
        ))
}

fn feed_matches_org_route(
    feed: &ResolveFeedView,
    org: &GithubOrgName,
    team: Option<&GithubTeamSlug>,
    filter: &OrgRouteFilter,
) -> bool {
    !feed.feed_id.starts_with("local:")
        && access_permits_org_route(&feed.access, feed.visibility.as_str(), org, team)
        && filter.stream_filter.permits_label(&feed.label)
}

fn headline_matches_org_route(
    headline: &RemoteHeadlineView,
    org: &GithubOrgName,
    team: Option<&GithubTeamSlug>,
    filter: &OrgRouteFilter,
) -> bool {
    !headline.feed_id.starts_with("local:")
        && access_permits_org_route(&headline.access, headline.visibility.as_str(), org, team)
        && filter.stream_filter.permits_label(&headline.feed_label)
        && headline_matches_reel_filter(headline, &filter.reel_filter)
        && !remote_copy_has_public_quality_issue(&format!(
            "{} {}",
            headline.headline, headline.deck
        ))
}

fn access_permits_org_route(
    access: &FeedAccessPolicy,
    visibility: &str,
    org: &GithubOrgName,
    team: Option<&GithubTeamSlug>,
) -> bool {
    match (visibility, team) {
        ("github_org", _) => access.permits_org(org),
        ("github_team", Some(team)) => access.permits_team(org, team),
        _ => false,
    }
}

fn headline_matches_reel_filter(headline: &RemoteHeadlineView, filter: &RemoteReelFilter) -> bool {
    if headline.score != 0 && headline.score < filter.min_score {
        return false;
    }
    let terms = headline_filter_terms(headline);
    filter_values_match(&filter.agent_kinds, &terms)
        && filter_values_match(&filter.story_kinds, &terms)
        && filter_values_match(&filter.project_tags, &terms)
}

fn filter_values_match(requested: &[String], terms: &[String]) -> bool {
    requested.is_empty()
        || requested.iter().any(|value| {
            let requested = normalize_filter_tag(value);
            !requested.is_empty() && terms.iter().any(|term| term == &requested)
        })
}

fn headline_filter_terms(headline: &RemoteHeadlineView) -> Vec<String> {
    let mut terms = headline
        .chips
        .iter()
        .map(|value| normalize_filter_tag(value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    for value in headline
        .lower_third
        .split(['/', '·', ',', '|'])
        .chain(std::iter::once(headline.feed_label.as_str()))
    {
        let value = normalize_filter_tag(value);
        if !value.is_empty() {
            terms.push(value);
        }
    }
    terms
}

fn normalize_filter_tag(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_sep = false;
    for ch in value.trim().trim_start_matches('@').chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep && !out.is_empty() {
            out.push('_');
            last_was_sep = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

fn remote_copy_has_public_quality_issue(copy: &str) -> bool {
    let normalized = copy.to_ascii_lowercase();
    [
        "production scaffold",
        "agent feed scaffold",
        "test gate",
        "test line",
        "tests remain red",
        "red run",
        "red runs",
        "failures remain",
        "fixture feed",
        "m0 signal path",
        "p2p capsule coverage advanced",
        "verification s",
        "command lifecycle captured",
        "without command output",
        "ci status",
        "checks ci status",
        "completed turn",
        "file-change pass",
        "planning state advanced",
        "plan update",
        "plan-state",
        "run state",
        "run state settled",
        "command event",
        "safe command",
        "shell check",
        "repository state",
        "records plan update",
        "shifts feed to edits",
        "two-file update",
        "codexci statusrun state",
        "confirms pass state",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
        || Regex::new(
            r"\b(?:[0-9]+|one|two|three|four|five|six|seven|eight|nine|ten)\s+(?:changed\s+)?files?\b|\bfiles?\s+changed\b",
        )
        .expect("file-count regex is valid")
        .is_match(&normalized)
        || (normalized.contains("tests passed") && !remote_copy_has_public_context(&normalized))
}

fn remote_copy_has_public_context(normalized: &str) -> bool {
    [
        "auth",
        "avatar",
        "browser",
        "broadcast",
        "capture",
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

#[cfg(test)]
fn network_snapshot_value(config: &EdgeConfig, snapshot: &SnapshotStore) -> serde_json::Value {
    network_snapshot_value_filtered(config, snapshot, None)
}

fn resolve_github_from_snapshot(
    config: &EdgeConfig,
    snapshot: &SnapshotStore,
    route: &RemoteUserRoute,
) -> Option<ResolveGithubResponse> {
    let login = route.login.as_str();
    let feeds = snapshot.feeds();
    let headlines = snapshot.headlines();
    let github_user_id = feeds
        .iter()
        .find(|feed| feed.publisher_login.eq_ignore_ascii_case(login))
        .and_then(|feed| feed.publisher_github_user_id)
        .or_else(|| {
            headlines
                .iter()
                .find(|headline| headline.publisher_login.eq_ignore_ascii_case(login))
                .and_then(|headline| headline.publisher_github_user_id)
        })?;
    let display_name = feeds
        .iter()
        .find(|feed| {
            feed.publisher_github_user_id == Some(github_user_id)
                || feed.publisher_login.eq_ignore_ascii_case(login)
        })
        .and_then(|feed| feed.publisher_display_name.clone())
        .or_else(|| {
            headlines
                .iter()
                .find(|headline| {
                    headline.publisher_github_user_id == Some(github_user_id)
                        || headline.publisher_login.eq_ignore_ascii_case(login)
                })
                .and_then(|headline| headline.publisher_display_name.clone())
        });
    let current_login = feeds
        .iter()
        .find(|feed| feed.publisher_github_user_id == Some(github_user_id))
        .map(|feed| feed.publisher_login.clone())
        .or_else(|| {
            headlines
                .iter()
                .find(|headline| headline.publisher_github_user_id == Some(github_user_id))
                .map(|headline| headline.publisher_login.clone())
        })
        .unwrap_or_else(|| route.login.to_string());
    let matching_feeds = feeds
        .into_iter()
        .filter(|feed| feed_matches_github_route(feed, github_user_id, &current_login, route))
        .collect::<Vec<_>>();
    let matching_headlines = headlines
        .into_iter()
        .filter(|headline| {
            headline_matches_github_route(headline, github_user_id, &current_login, route)
        })
        .collect::<Vec<_>>();
    if matching_feeds.is_empty() && matching_headlines.is_empty() {
        return None;
    }
    let mut response = ResolveGithubResponse {
        state: "resolved".to_string(),
        network_id: route.network.network_id(),
        compatibility: ProtocolCompatibility::current(),
        requested_login: route.login.to_string(),
        github_user_id,
        profile: GithubProfileView {
            login: current_login,
            name: display_name,
            avatar: Some(config.github_avatar_url(github_user_id)),
        },
        feeds: matching_feeds,
        headlines: matching_headlines,
        browser_seed_url: "/browser-seed".to_string(),
        expires_at: OffsetDateTime::now_utc() + Duration::minutes(15),
        signature: agent_feed_p2p_proto::Signature::unsigned().digest,
    };
    merge_resolve_feed_views(
        &mut response.feeds,
        feed_views_from_headlines(response.headlines.clone()),
    );
    Some(response)
}

fn network_snapshot_value_filtered(
    config: &EdgeConfig,
    snapshot: &SnapshotStore,
    filter: Option<&RemoteReelFilter>,
) -> serde_json::Value {
    let headlines = snapshot
        .headlines()
        .into_iter()
        .filter(|headline| {
            filter
                .map(|filter| headline_matches_reel_filter(headline, filter))
                .unwrap_or(true)
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "state": "ready",
        "product": "feed",
        "network_id": config.network_id,
        "compatibility": ProtocolCompatibility::current(),
        "edge_base_url": config.edge_domain,
        "browser_app_base_url": config.browser_app_base_url,
        "bootstrap_peers": config.bootstrap_peers,
        "bootstrap_topology": "single_bootstrap",
        "data_plane": "edge_snapshot_fallback",
        "feed_mode": "discovery",
        "story_only": true,
        "raw_events": false,
        "feeds": snapshot.feeds(),
        "headlines": headlines,
    })
}

fn requested_network_id(value: Option<&str>, config: &EdgeConfig) -> String {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("mainnet") => config.network_id.clone(),
        Some(value) => value.to_string(),
    }
}

fn edge_error(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({
            "state": "error",
            "error": message.into(),
        })),
    )
        .into_response()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VerifiedSession {
    github_user_id: u64,
    scopes: BTreeSet<String>,
    github_orgs: BTreeSet<String>,
    github_teams: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct VerifiedBearer {
    session: VerifiedSession,
    token: String,
}

impl VerifiedSession {
    fn permits_org(&self, org: &GithubOrgName) -> bool {
        self.github_orgs
            .contains(&org.as_str().to_ascii_lowercase())
    }

    fn permits_team(&self, org: &GithubOrgName, team: &GithubTeamSlug) -> bool {
        self.github_teams.contains(&format!(
            "{}/{}",
            org.as_str().to_ascii_lowercase(),
            team.as_str().to_ascii_lowercase()
        ))
    }

    fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }
}

fn issue_session_token_for_auth(
    github_user_id: u64,
    expires_at: OffsetDateTime,
    scopes: Vec<String>,
    github_orgs: Vec<String>,
    github_teams: Vec<String>,
) -> Option<String> {
    let secret = session_secret()?;
    let mut scopes = scopes
        .into_iter()
        .map(|scope| scope.trim().to_string())
        .filter(|scope| !scope.is_empty())
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    let mut github_orgs = github_orgs
        .into_iter()
        .map(|org| org.trim().to_ascii_lowercase())
        .filter(|org| !org.is_empty())
        .collect::<Vec<_>>();
    github_orgs.sort();
    github_orgs.dedup();
    let mut github_teams = github_teams
        .into_iter()
        .map(|team| team.trim().to_ascii_lowercase())
        .filter(|team| !team.is_empty())
        .collect::<Vec<_>>();
    github_teams.sort();
    github_teams.dedup();
    let claims = SessionTokenClaims {
        github_user_id,
        expires: expires_at.unix_timestamp(),
        scopes,
        github_orgs,
        github_teams,
    };
    issue_session_token_with_claims(&claims, &secret)
}

fn verify_bearer_session(headers: &HeaderMap) -> Option<VerifiedBearer> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let token = value.strip_prefix("Bearer ")?.trim();
    let session = verify_session_token(token)?;
    Some(VerifiedBearer {
        session,
        token: token.to_string(),
    })
}

fn verify_session_token(token: &str) -> Option<VerifiedSession> {
    let secret = session_secret()?;
    verify_session_token_with_secret(token, &secret, OffsetDateTime::now_utc().unix_timestamp())
}

#[cfg(test)]
fn issue_session_token_with_secret(github_user_id: u64, expires: i64, secret: &str) -> String {
    let sig = session_signature(github_user_id, expires, secret);
    format!("feed.{github_user_id}.{expires}.{sig}")
}

fn verify_session_token_with_secret(
    token: &str,
    secret: &str,
    now_unix: i64,
) -> Option<VerifiedSession> {
    let mut parts = token.split('.');
    if parts.next()? != "feed" {
        return verify_session_token_v2_with_secret(token, secret, now_unix);
    }
    let github_user_id = parts.next()?.parse::<u64>().ok()?;
    let expires = parts.next()?.parse::<i64>().ok()?;
    let signature = parts.next()?;
    if parts.next().is_some() || expires <= now_unix {
        return None;
    }
    let expected = session_signature(github_user_id, expires, secret);
    constant_time_eq(signature.as_bytes(), expected.as_bytes()).then_some(VerifiedSession {
        github_user_id,
        scopes: BTreeSet::new(),
        github_orgs: BTreeSet::new(),
        github_teams: BTreeSet::new(),
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SessionTokenClaims {
    github_user_id: u64,
    expires: i64,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    github_orgs: Vec<String>,
    #[serde(default)]
    github_teams: Vec<String>,
}

fn issue_session_token_with_claims(claims: &SessionTokenClaims, secret: &str) -> Option<String> {
    let payload = serde_json::to_vec(claims).ok()?;
    let payload = hex_encode(&payload);
    let sig = session_payload_signature(&payload, secret);
    Some(format!("feedv2.{payload}.{sig}"))
}

fn verify_session_token_v2_with_secret(
    token: &str,
    secret: &str,
    now_unix: i64,
) -> Option<VerifiedSession> {
    let mut parts = token.split('.');
    if parts.next()? != "feedv2" {
        return None;
    }
    let payload = parts.next()?;
    let signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let expected = session_payload_signature(payload, secret);
    if !constant_time_eq(signature.as_bytes(), expected.as_bytes()) {
        return None;
    }
    let bytes = hex_decode(payload).ok()?;
    let claims: SessionTokenClaims = serde_json::from_slice(&bytes).ok()?;
    if claims.expires <= now_unix {
        return None;
    }
    Some(VerifiedSession {
        github_user_id: claims.github_user_id,
        scopes: claims.scopes.into_iter().collect(),
        github_orgs: claims.github_orgs.into_iter().collect(),
        github_teams: claims.github_teams.into_iter().collect(),
    })
}

fn session_secret() -> Option<String> {
    env::var("AGENT_FEED_EDGE_SESSION_SECRET")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(github_client_secret)
}

fn session_signature(github_user_id: u64, expires: i64, secret: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac accepts arbitrary key lengths");
    mac.update(b"agent-feed-session-v1");
    mac.update(github_user_id.to_string().as_bytes());
    mac.update(b":");
    mac.update(expires.to_string().as_bytes());
    hex_lower(&mac.finalize().into_bytes())
}

fn session_payload_signature(payload: &str, secret: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac accepts arbitrary key lengths");
    mac.update(b"agent-feed-session-v2");
    mac.update(payload.as_bytes());
    hex_lower(&mac.finalize().into_bytes())
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GithubOauthState {
    client: String,
    state: String,
    redirect_uri: Option<String>,
    return_to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github_org: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubTokenResponse {
    access_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GithubAccessToken {
    access_token: String,
    scopes: Vec<String>,
}

fn github_client_id() -> Option<String> {
    env::var("AGENT_FEED_GITHUB_CLIENT_ID")
        .ok()
        .or_else(|| env::var("GITHUB_CLIENT_ID").ok())
        .filter(|value| !value.trim().is_empty())
}

fn github_client_secret() -> Option<String> {
    env::var("AGENT_FEED_GITHUB_CLIENT_SECRET")
        .ok()
        .or_else(|| env::var("GITHUB_CLIENT_SECRET").ok())
        .filter(|value| !value.trim().is_empty())
}

fn default_github_scope(policy: &OrgDeploymentPolicy) -> String {
    if policy.is_restricted() {
        "read:user read:org".to_string()
    } else {
        "read:user".to_string()
    }
}

fn authorize_github_token_policy(
    access_token: &str,
    profile: &agent_feed_identity_github::GithubProfile,
    policy: &OrgDeploymentPolicy,
) -> Result<(), String> {
    let Some(org) = policy.required_org.as_ref() else {
        return Ok(());
    };
    fetch_github_org_membership(access_token, org, profile.login.as_str())?;
    if policy.required_teams.is_empty() {
        return Ok(());
    }
    let teams = fetch_github_user_teams(access_token)?;
    for required in &policy.required_teams {
        if !teams
            .iter()
            .any(|team| team.org == *org && team.slug == *required)
        {
            return Err(format!("github team membership required: {org}/{required}"));
        }
    }
    Ok(())
}

fn fetch_github_org_membership(
    access_token: &str,
    org: &GithubOrgName,
    _login: &str,
) -> Result<(), String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "accept: application/vnd.github+json",
            "-H",
            &format!("authorization: Bearer {access_token}"),
            "-H",
            "user-agent: feed-edge",
            &format!("https://api.github.com/user/memberships/orgs/{org}"),
        ])
        .output()
        .map_err(|err| format!("github org membership check failed to start: {err}"))?;
    if !output.status.success() {
        return Err(format!("github org membership required: {org}"));
    }
    let response: GithubOrgMembershipResponse = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("github org membership response failed: {err}"))?;
    if response.state.as_deref() == Some("active") {
        Ok(())
    } else {
        Err(format!("github org membership required: {org}"))
    }
}

#[derive(Debug, Deserialize)]
struct GithubOrgMembershipResponse {
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubTeamMembershipResponse {
    slug: String,
    organization: GithubTeamOrgResponse,
}

#[derive(Debug, Deserialize)]
struct GithubTeamOrgResponse {
    login: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GithubTeamMembership {
    org: GithubOrgName,
    slug: GithubTeamSlug,
}

fn fetch_github_user_teams(access_token: &str) -> Result<Vec<GithubTeamMembership>, String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "accept: application/vnd.github+json",
            "-H",
            &format!("authorization: Bearer {access_token}"),
            "-H",
            "user-agent: feed-edge",
            "https://api.github.com/user/teams",
        ])
        .output()
        .map_err(|err| format!("github team list failed to start: {err}"))?;
    if !output.status.success() {
        return Err(format!("github team list exited with {}", output.status));
    }
    let response: Vec<GithubTeamMembershipResponse> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("github team response failed: {err}"))?;
    Ok(response
        .into_iter()
        .filter_map(|team| {
            Some(GithubTeamMembership {
                org: GithubOrgName::parse(team.organization.login).ok()?,
                slug: GithubTeamSlug::parse(team.slug).ok()?,
            })
        })
        .collect())
}

fn is_loopback_callback(uri: &str) -> bool {
    uri.starts_with("http://127.0.0.1:")
        || uri.starts_with("http://localhost:")
        || uri.starts_with("http://[::1]:")
}

fn is_allowed_browser_return(config: &EdgeConfig, return_to: &str) -> bool {
    let base = config.browser_app_base_url.trim_end_matches('/');
    return_to == base || return_to.starts_with(&format!("{base}/"))
}

fn origin_of_url(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let host = rest.split('/').next()?;
    if host.is_empty() {
        return None;
    }
    let scheme = if url.starts_with("https://") {
        "https"
    } else if url.starts_with("http://") {
        "http"
    } else {
        return None;
    };
    Some(format!("{scheme}://{host}"))
}

fn encode_oauth_state(payload: &GithubOauthState) -> Result<String, serde_json::Error> {
    Ok(format!(
        "feed:{}",
        hex_encode(&serde_json::to_vec(payload)?)
    ))
}

fn decode_oauth_state(value: &str) -> Result<GithubOauthState, String> {
    let Some(hex) = value.strip_prefix("feed:") else {
        return Err("github callback state is not a feed state".to_string());
    };
    let bytes = hex_decode(hex)?;
    serde_json::from_slice(&bytes).map_err(|err| format!("github callback state failed: {err}"))
}

fn exchange_github_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<GithubAccessToken, String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-X",
            "POST",
            "-H",
            "accept: application/json",
            "-H",
            "user-agent: feed-edge",
            "--data-urlencode",
            &format!("client_id={client_id}"),
            "--data-urlencode",
            &format!("client_secret={client_secret}"),
            "--data-urlencode",
            &format!("code={code}"),
            "--data-urlencode",
            &format!("redirect_uri={redirect_uri}"),
            "https://github.com/login/oauth/access_token",
        ])
        .output()
        .map_err(|err| format!("github token exchange failed to start: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "github token exchange exited with {}",
            output.status
        ));
    }
    let response: GithubTokenResponse = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("github token response failed: {err}"))?;
    if let Some(error) = response.error {
        return Err(response.error_description.unwrap_or(error));
    }
    let scopes = response
        .scope
        .unwrap_or_default()
        .split(|value| value == ',' || value == ' ')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let access_token = response
        .access_token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "github token response missing access token".to_string())?;
    Ok(GithubAccessToken {
        access_token,
        scopes,
    })
}

fn fetch_github_profile(
    access_token: &str,
) -> Result<agent_feed_identity_github::GithubProfile, String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "accept: application/vnd.github+json",
            "-H",
            &format!("authorization: Bearer {access_token}"),
            "-H",
            "user-agent: feed-edge",
            "https://api.github.com/user",
        ])
        .output()
        .map_err(|err| format!("github profile fetch failed to start: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "github profile fetch exited with {}",
            output.status
        ));
    }
    let response: GithubUserResponse = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("github profile response failed: {err}"))?;
    response
        .try_into()
        .map_err(|err: GithubResolveError| err.to_string())
}

fn encode_query(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn hex_decode(value: &str) -> Result<Vec<u8>, String> {
    if !value.len().is_multiple_of(2) {
        return Err("github callback state has invalid length".to_string());
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks(2) {
        let text = std::str::from_utf8(chunk)
            .map_err(|err| format!("github callback state is not utf8: {err}"))?;
        let byte = u8::from_str_radix(text, 16)
            .map_err(|err| format!("github callback state is not hex: {err}"))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

#[derive(Clone, Copy, Debug)]
pub struct CurlGithubResolver;

impl GithubResolver for CurlGithubResolver {
    fn resolve_login(
        &self,
        login: &GithubLogin,
    ) -> Result<agent_feed_identity_github::GithubProfile, GithubResolveError> {
        let url = format!("https://api.github.com/users/{login}");
        let output = Command::new("curl")
            .args([
                "-fsSL",
                "-H",
                "accept: application/vnd.github+json",
                "-H",
                "user-agent: feed-edge",
                &url,
            ])
            .output()
            .map_err(|err| GithubResolveError::Unavailable(err.to_string()))?;
        if !output.status.success() {
            return match output.status.code() {
                Some(22) => Err(GithubResolveError::NotFound(login.to_string())),
                _ => Err(GithubResolveError::Unavailable(format!(
                    "github api exited with {}",
                    output.status
                ))),
            };
        }
        let response: GithubUserResponse = serde_json::from_slice(&output.stdout)
            .map_err(|err| GithubResolveError::Unavailable(err.to_string()))?;
        response.try_into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_core::{PrivacyClass, Severity};
    use agent_feed_directory::{FeedDirectoryEntry, GithubPrincipal, RemoteUserRoute};
    use agent_feed_identity::{GithubOrgName, GithubTeamSlug, GithubUserId};
    use agent_feed_identity_github::{GithubProfile, StaticGithubAccessResolver};
    use agent_feed_p2p_proto::{FeedVisibility, Signed, StoryCapsule};
    use agent_feed_story::{CompiledStory, StoryFamily, StoryKey};

    fn profile() -> GithubProfile {
        GithubProfile {
            id: GithubUserId::new(123),
            login: GithubLogin::parse("mosure").expect("login parses"),
            name: Some("mosure".to_string()),
            avatar_url: Some("https://avatars.githubusercontent.com/u/123?v=4".to_string()),
        }
    }

    fn config() -> EdgeConfig {
        EdgeConfig {
            org_policy: OrgDeploymentPolicy::default(),
            ..EdgeConfig::mainnet()
        }
    }

    fn resolver_with_directory() -> EdgeResolver<StaticGithubResolver> {
        let profile = profile();
        let owner = GithubPrincipal::from_profile(&profile, "edge");
        let entry = FeedDirectoryEntry::new(
            "agent-feed-mainnet",
            "feed-workstation",
            owner,
            "peer-a",
            "workstation",
            FeedVisibility::Public,
            1,
        )
        .sign("peer-a")
        .expect("entry signs");
        let mut directory = DirectoryStore::new();
        directory.publish(entry).expect("entry publishes");
        EdgeResolver::new(
            config(),
            StaticGithubResolver::new().with_profile(profile),
            directory,
        )
    }

    fn org_entry(
        profile: &GithubProfile,
        feed_id: &str,
        peer_id: &str,
        team: Option<&str>,
    ) -> FeedDirectoryEntry {
        let owner = GithubPrincipal::from_profile(profile, "edge");
        let entry = FeedDirectoryEntry::new(
            "agent-feed-mainnet",
            feed_id,
            owner,
            peer_id,
            "workstation",
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

    #[test]
    fn edge_resolves_login_to_signed_discovery_ticket() {
        let edge = resolver_with_directory();
        let ticket = edge
            .resolve_github_route("/mosure", Some("all"))
            .expect("route resolves");

        assert_eq!(ticket.resolved_github_id, GithubUserId::new(123));
        assert_eq!(ticket.candidate_feeds.len(), 1);
        assert!(ticket.verify_signature().expect("ticket verifies"));
        assert!(!ticket.rendezvous_namespaces[0].contains("mosure"));
        assert!(!ticket.rendezvous_namespaces[0].contains("123"));
    }

    #[test]
    fn edge_response_matches_public_api_shape() {
        let edge = resolver_with_directory();
        let ticket = edge
            .resolve_github_route("/@mosure", Some("streams=workstation"))
            .expect("route resolves");
        let response = ResolveGithubResponse::from_ticket(&ticket, &config());

        assert_eq!(response.state, "resolved");
        assert_eq!(response.requested_login, "mosure");
        assert_eq!(response.github_user_id, 123);
        assert_eq!(
            response.compatibility.protocol_version,
            agent_feed_p2p_proto::AGENT_FEED_PROTOCOL_VERSION
        );
        assert_eq!(
            response.compatibility.model_version,
            agent_feed_p2p_proto::AGENT_FEED_MODEL_VERSION
        );
        assert_eq!(response.browser_seed_url, "/browser-seed");
        assert_eq!(
            response.profile.avatar.as_deref(),
            Some("https://api.feed.aberration.technology/avatar/github/123")
        );
        assert_eq!(response.feeds[0].publisher_login, "mosure");
        assert_eq!(
            response.feeds[0].compatibility.protocol_version,
            agent_feed_p2p_proto::AGENT_FEED_PROTOCOL_VERSION
        );
        assert_eq!(
            response.feeds[0].publisher_avatar.as_deref(),
            Some("https://api.feed.aberration.technology/avatar/github/123")
        );
        assert!(response.feeds[0].publisher_verified);
    }

    #[test]
    fn edge_rejects_wrong_requested_network() {
        let edge = resolver_with_directory();
        let err = edge
            .resolve_github_route("/mosure", Some("network=agent-feed-lab"))
            .expect_err("wrong network is rejected");

        assert!(matches!(
            err,
            EdgeError::Directory(DirectoryError::NetworkMismatch { .. })
        ));
    }

    #[test]
    fn avatar_endpoint_urls_are_edge_safe() {
        let config = config();

        assert_eq!(
            config.github_avatar_url(123),
            "https://api.feed.aberration.technology/avatar/github/123"
        );
        assert_eq!(
            github_avatar_upstream_url(123),
            "https://avatars.githubusercontent.com/u/123?v=4&s=192"
        );
    }

    #[test]
    fn github_oauth_callback_uses_edge_host() {
        let config = config();

        assert_eq!(
            config.github_callback_url(),
            "https://api.feed.aberration.technology/callback/github"
        );
        assert_ne!(config.github_callback_url(), config.browser_app_base_url);
    }

    #[test]
    fn fabric_probe_payload_is_display_safe() {
        let payload = fabric_probe_payload(&config(), "tcp");

        assert!(payload.contains("\"product\":\"feed\""));
        assert!(payload.contains("\"protocol\":\"agent-feed.edge/1\""));
        assert!(payload.contains("\"protocol_version\":1"));
        assert!(payload.contains("\"model_version\":3"));
        assert!(payload.contains("\"transport\":\"tcp\""));
        assert!(payload.contains("\"state\":\"ready\""));
        assert!(!payload.contains("secret"));
        assert!(!payload.contains("token"));
    }

    #[test]
    fn network_snapshot_supports_global_discovery_shape() {
        let store = SnapshotStore::default();
        let snapshot = network_snapshot_value(&config(), &store);

        assert_eq!(snapshot["state"], "ready");
        assert_eq!(snapshot["feed_mode"], "discovery");
        assert_eq!(snapshot["story_only"], true);
        assert_eq!(snapshot["raw_events"], false);
        assert!(snapshot["feeds"].as_array().is_some());
        assert!(snapshot["headlines"].as_array().is_some());
        assert_eq!(snapshot["compatibility"]["protocol_version"], 1);
    }

    #[test]
    fn network_snapshot_query_resolves_mainnet_alias_only() {
        let config = config();

        assert_eq!(
            requested_network_id(Some("mainnet"), &config),
            "agent-feed-mainnet"
        );
        assert!(
            ensure_network_id(
                &config.network_id,
                &requested_network_id(Some("agent-feed-mainnet"), &config)
            )
            .is_ok()
        );
        assert!(matches!(
            ensure_network_id(
                &config.network_id,
                &requested_network_id(Some("agent-feed-lab"), &config)
            ),
            Err(DirectoryError::NetworkMismatch { .. })
        ));
    }

    #[test]
    fn network_publish_request_requires_compatibility_metadata() {
        let old_request: NetworkPublishRequest =
            serde_json::from_value(serde_json::json!({"feed_name":"workstation","capsules":[]}))
                .expect("old shape parses");
        assert!(old_request.network_id.is_none());
        assert!(old_request.compatibility.is_none());

        let current_request: NetworkPublishRequest = serde_json::from_value(serde_json::json!({
            "network_id": "agent-feed-mainnet",
            "compatibility": ProtocolCompatibility::current(),
            "feed_name":"workstation",
            "capsules":[],
        }))
        .expect("current shape parses");
        assert_eq!(
            current_request.network_id.as_deref(),
            Some("agent-feed-mainnet")
        );
        assert_eq!(
            current_request.compatibility,
            Some(ProtocolCompatibility::current())
        );
    }

    #[test]
    fn signed_session_tokens_verify_and_reject_tamper() {
        let token = issue_session_token_with_secret(123, 4_102_444_800, "secret");

        assert_eq!(
            verify_session_token_with_secret(&token, "secret", 1_767_225_600),
            Some(VerifiedSession {
                github_user_id: 123,
                scopes: BTreeSet::new(),
                github_orgs: BTreeSet::new(),
                github_teams: BTreeSet::new(),
            })
        );
        assert_eq!(
            verify_session_token_with_secret(&token, "wrong", 1_767_225_600),
            None
        );
        assert_eq!(
            verify_session_token_with_secret(&token, "secret", 4_102_444_801),
            None
        );

        let claims = SessionTokenClaims {
            github_user_id: 123,
            expires: 4_102_444_800,
            scopes: vec!["read:user".to_string(), "read:org".to_string()],
            github_orgs: vec!["aberration-technology".to_string()],
            github_teams: vec!["aberration-technology/release".to_string()],
        };
        let token = issue_session_token_with_claims(&claims, "secret").expect("token signs");
        let session = verify_session_token_with_secret(&token, "secret", 1_767_225_600)
            .expect("token verifies");
        let org = GithubOrgName::parse("aberration-technology").expect("org parses");
        let team = GithubTeamSlug::parse("release").expect("team parses");
        assert_eq!(session.github_user_id, 123);
        assert!(session.has_scope("read:org"));
        assert!(session.permits_org(&org));
        assert!(session.permits_team(&org, &team));
    }

    #[test]
    fn network_snapshot_store_dedupes_and_exposes_headlines() {
        let store = SnapshotStore::default();
        let headline = RemoteHeadlineView {
            feed_id: "github:123:workstation".to_string(),
            feed_label: "workstation".to_string(),
            compatibility: ProtocolCompatibility::current(),
            visibility: "public".to_string(),
            access: FeedAccessPolicy::public(),
            created_at: Some(OffsetDateTime::now_utc()),
            publisher_github_user_id: Some(123),
            publisher_login: "mosure".to_string(),
            publisher_display_name: Some("mosure".to_string()),
            publisher_avatar: Some("/avatar/github/123".to_string()),
            verified: true,
            headline: "codex finished release pass".to_string(),
            deck: "settled story capsule reached the edge.".to_string(),
            lower_third: "@mosure / workstation".to_string(),
            chips: vec!["verified".to_string(), "codex".to_string()],
            score: 84,
            image: None,
        };

        store.push(headline.clone());
        store.push(headline);
        let snapshot = network_snapshot_value(&config(), &store);

        assert_eq!(
            snapshot["headlines"].as_array().expect("headlines").len(),
            1
        );
        assert_eq!(
            snapshot["headlines"][0]["publisher_login"],
            serde_json::json!("mosure")
        );
        assert_eq!(
            snapshot["headlines"][0]["publisher_github_user_id"],
            serde_json::json!(123)
        );
        assert_eq!(
            snapshot["headlines"][0]["headline"],
            serde_json::json!("codex finished release pass")
        );
        assert!(snapshot["headlines"][0]["created_at"].is_string());
        assert_eq!(
            snapshot["feeds"][0]["publisher_login"],
            serde_json::json!("mosure")
        );
    }

    #[test]
    fn network_snapshot_store_prunes_stale_headlines_and_feeds() {
        let store = SnapshotStore::default();
        let stale_time = OffsetDateTime::now_utc() - Duration::hours(2);
        let stale_headline = RemoteHeadlineView {
            feed_id: "github:123:workstation".to_string(),
            feed_label: "workstation".to_string(),
            compatibility: ProtocolCompatibility::current(),
            visibility: "public".to_string(),
            access: FeedAccessPolicy::public(),
            created_at: Some(stale_time),
            publisher_github_user_id: Some(123),
            publisher_login: "mosure".to_string(),
            publisher_display_name: Some("mosure".to_string()),
            publisher_avatar: Some("/avatar/github/123".to_string()),
            verified: true,
            headline: "old release pass loops forever".to_string(),
            deck: "this stale story should not remain display material.".to_string(),
            lower_third: "@mosure / workstation".to_string(),
            chips: vec!["verified".to_string()],
            score: 84,
            image: None,
        };
        let stale_feed = ResolveFeedView {
            feed_id: "github:123:workstation".to_string(),
            label: "workstation".to_string(),
            visibility: "public".to_string(),
            access: FeedAccessPolicy::public(),
            compatibility: ProtocolCompatibility::current(),
            publisher_github_user_id: Some(123),
            publisher_login: "mosure".to_string(),
            publisher_display_name: Some("mosure".to_string()),
            publisher_avatar: Some("/avatar/github/123".to_string()),
            publisher_verified: true,
            last_seen_at: stale_time,
        };

        assert!(!store.push(stale_headline));
        store.upsert_feed(stale_feed);
        let snapshot = network_snapshot_value(&config(), &store);

        assert_eq!(
            snapshot["headlines"].as_array().expect("headlines").len(),
            0
        );
        assert_eq!(snapshot["feeds"].as_array().expect("feeds").len(), 0);
    }

    #[test]
    fn network_snapshot_filters_headlines_by_project_and_score() {
        let store = SnapshotStore::default();
        let headline = RemoteHeadlineView {
            feed_id: "github:123:workstation".to_string(),
            feed_label: "workstation".to_string(),
            compatibility: ProtocolCompatibility::current(),
            visibility: "public".to_string(),
            access: FeedAccessPolicy::public(),
            created_at: Some(OffsetDateTime::now_utc()),
            publisher_github_user_id: Some(123),
            publisher_login: "mosure".to_string(),
            publisher_display_name: Some("mosure".to_string()),
            publisher_avatar: Some("/avatar/github/123".to_string()),
            verified: true,
            headline: "feed publish path exposes delivery status".to_string(),
            deck: "operators can see settled stories on the public edge.".to_string(),
            lower_third: "@mosure / workstation".to_string(),
            chips: vec!["agent_reel".to_string(), "codex".to_string()],
            score: 84,
            image: None,
        };
        let wrong_project = RemoteHeadlineView {
            chips: vec!["burn_p2p".to_string(), "codex".to_string()],
            headline: "burn p2p route advertises separate project work".to_string(),
            ..headline.clone()
        };
        let low_score = RemoteHeadlineView {
            score: 70,
            headline: "feed headline stays below route threshold".to_string(),
            ..headline.clone()
        };
        store.push(headline);
        store.push(wrong_project);
        store.push(low_score);
        let filter = RemoteReelFilter::from_query(Some("projects=agent-reel&min_score=80"))
            .expect("filter parses");
        let snapshot = network_snapshot_value_filtered(&config(), &store, Some(&filter));
        let headlines = snapshot["headlines"].as_array().expect("headlines");

        assert_eq!(headlines.len(), 1);
        assert_eq!(
            headlines[0]["chips"],
            serde_json::json!(["agent_reel", "codex"])
        );
    }

    #[test]
    fn network_snapshot_hides_local_and_bad_public_copy() {
        let store = SnapshotStore::default();
        let base = RemoteHeadlineView {
            feed_id: "github:123:workstation".to_string(),
            feed_label: "workstation".to_string(),
            compatibility: ProtocolCompatibility::current(),
            visibility: "public".to_string(),
            access: FeedAccessPolicy::public(),
            created_at: Some(OffsetDateTime::now_utc()),
            publisher_github_user_id: Some(123),
            publisher_login: "mosure".to_string(),
            publisher_display_name: Some("mosure".to_string()),
            publisher_avatar: Some("/avatar/github/123".to_string()),
            verified: true,
            headline: "codex finished release pass".to_string(),
            deck: "settled story capsule reached the edge.".to_string(),
            lower_third: "@mosure / workstation".to_string(),
            chips: vec!["verified".to_string(), "codex".to_string()],
            score: 84,
            image: None,
        };
        let mut local = base.clone();
        local.feed_id = "local:workstation".to_string();
        let mut bad = base.clone();
        bad.headline = "Codex advances production scaffold; test gate stays red".to_string();
        let mut generic = base.clone();
        generic.headline = "codex moved feed work forward".to_string();
        generic.deck = "p2p capsule coverage advanced; tests passed.".to_string();
        let mut capture_placeholder = base.clone();
        capture_placeholder.headline =
            "codex command lifecycle captured without command output".to_string();
        let mut ci_status = base.clone();
        ci_status.headline = "codex checks ci status, settles run state".to_string();
        ci_status.deck =
            "sixteen command events converged on ci status and left the run state settled."
                .to_string();
        let mut file_count = base.clone();
        file_count.headline = "codex changes two files, shifts feed to edits".to_string();
        file_count.deck = "two files changed after the prior ci status summary.".to_string();
        let mut test_status = base.clone();
        test_status.headline = "codex verifies tests, confirms pass state".to_string();
        test_status.deck = "tests passed after the two-file change.".to_string();
        let mut contextual_with_file_chip = base.clone();
        contextual_with_file_chip.headline =
            "gemma bridge now isolates the device mismatch blocking inference".to_string();
        contextual_with_file_chip.deck =
            "input tensors are routed toward the quantized model path so forward checks can reach TRELLIS conditioning."
                .to_string();
        contextual_with_file_chip.chips = vec![
            "gemma4_trellis2".to_string(),
            "codex".to_string(),
            "turn".to_string(),
            "4 files".to_string(),
            "score 84".to_string(),
        ];

        store.push(local);
        store.push(bad);
        store.push(generic);
        store.push(capture_placeholder);
        store.push(ci_status);
        store.push(file_count);
        store.push(test_status);
        store.push(contextual_with_file_chip.clone());
        store.push(base);
        let snapshot = network_snapshot_value(&config(), &store);

        let headlines = snapshot["headlines"].as_array().expect("headlines");
        assert_eq!(headlines.len(), 2);
        assert!(headlines.iter().any(
            |headline| headline["headline"] == serde_json::json!("codex finished release pass")
        ));
        assert!(headlines.iter().any(|headline| headline["headline"]
            == serde_json::json!(contextual_with_file_chip.headline)));
        let feeds = snapshot["feeds"].as_array().expect("feeds");
        assert!(feeds.iter().all(|feed| {
            !feed["feed_id"]
                .as_str()
                .expect("feed id")
                .starts_with("local:")
        }));
    }

    #[test]
    fn network_snapshot_store_exposes_feed_presence_before_headline() {
        let store = SnapshotStore::default();
        store.upsert_feed(ResolveFeedView {
            feed_id: "github:123:workstation".to_string(),
            label: "workstation".to_string(),
            compatibility: ProtocolCompatibility::current(),
            visibility: "public".to_string(),
            access: FeedAccessPolicy::public(),
            publisher_github_user_id: Some(123),
            publisher_login: "mosure".to_string(),
            publisher_display_name: Some("mosure".to_string()),
            publisher_avatar: Some("/avatar/github/123".to_string()),
            publisher_verified: true,
            last_seen_at: OffsetDateTime::now_utc(),
        });

        let snapshot = network_snapshot_value(&config(), &store);

        assert_eq!(
            snapshot["headlines"].as_array().expect("headlines").len(),
            0
        );
        assert_eq!(snapshot["feeds"].as_array().expect("feeds").len(), 1);
        assert_eq!(
            snapshot["feeds"][0]["publisher_github_user_id"],
            serde_json::json!(123)
        );
        assert_eq!(
            snapshot["feeds"][0]["publisher_login"],
            serde_json::json!("mosure")
        );
    }

    #[test]
    fn github_resolver_can_fall_back_to_snapshot_identity() {
        let store = SnapshotStore::default();
        store.upsert_feed(ResolveFeedView {
            feed_id: "github:123:workstation".to_string(),
            label: "workstation".to_string(),
            compatibility: ProtocolCompatibility::current(),
            visibility: "public".to_string(),
            access: FeedAccessPolicy::public(),
            publisher_github_user_id: Some(123),
            publisher_login: "mosure".to_string(),
            publisher_display_name: Some("mitchell mosure".to_string()),
            publisher_avatar: Some("/avatar/github/123".to_string()),
            publisher_verified: true,
            last_seen_at: OffsetDateTime::now_utc(),
        });
        let route =
            RemoteUserRoute::parse("/mosure", Some("streams=workstation")).expect("route parses");

        let response =
            resolve_github_from_snapshot(&config(), &store, &route).expect("snapshot resolves");

        assert_eq!(response.state, "resolved");
        assert_eq!(response.github_user_id, 123);
        assert_eq!(response.profile.login, "mosure");
        assert_eq!(response.feeds.len(), 1);
        assert_eq!(response.feeds[0].label, "workstation");
    }

    #[tokio::test]
    async fn network_publish_presence_registers_verified_feed_without_headline() {
        let state = Arc::new(HttpState {
            config: config(),
            snapshot: SnapshotStore::default(),
        });
        let request = NetworkPublishRequest {
            network_id: Some("agent-feed-mainnet".to_string()),
            compatibility: Some(ProtocolCompatibility::current()),
            feed_name: Some("workstation".to_string()),
            feed_id: Some("github:123:workstation".to_string()),
            visibility: Some(FeedVisibility::Public),
            github_org: None,
            github_team: None,
            publisher: Some(PublisherIdentity::github(
                123,
                "mosure",
                Some("mosure".to_string()),
                Some("/avatar/github/123".to_string()),
            )),
            capsules: Vec::new(),
        };
        let response = network_publish_verified(
            state.clone(),
            VerifiedBearer {
                session: VerifiedSession {
                    github_user_id: 123,
                    scopes: BTreeSet::new(),
                    github_orgs: BTreeSet::new(),
                    github_teams: BTreeSet::new(),
                },
                token: "test-session-token".to_string(),
            },
            request,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = network_snapshot_value(&state.config, &state.snapshot);
        assert_eq!(
            snapshot["headlines"].as_array().expect("headlines").len(),
            0
        );
        assert_eq!(snapshot["feeds"].as_array().expect("feeds").len(), 1);
        assert_eq!(
            snapshot["feeds"][0]["feed_id"],
            serde_json::json!("github:123:workstation")
        );
        assert_eq!(
            snapshot["feeds"][0]["publisher_login"],
            serde_json::json!("mosure")
        );
    }

    #[tokio::test]
    async fn network_publish_private_org_feed_stays_out_of_public_snapshot() {
        let state = Arc::new(HttpState {
            config: config(),
            snapshot: SnapshotStore::default(),
        });
        let request = NetworkPublishRequest {
            network_id: Some("agent-feed-mainnet".to_string()),
            compatibility: Some(ProtocolCompatibility::current()),
            feed_name: Some("workstation".to_string()),
            feed_id: Some("github:123:workstation".to_string()),
            visibility: Some(FeedVisibility::GithubOrg),
            github_org: Some("aberration-technology".to_string()),
            github_team: None,
            publisher: Some(PublisherIdentity::github(
                123,
                "mosure",
                Some("mosure".to_string()),
                Some("/avatar/github/123".to_string()),
            )),
            capsules: Vec::new(),
        };

        let denied = network_publish_verified(
            state.clone(),
            VerifiedBearer {
                session: VerifiedSession {
                    github_user_id: 123,
                    scopes: BTreeSet::new(),
                    github_orgs: BTreeSet::new(),
                    github_teams: BTreeSet::new(),
                },
                token: "test-session-token".to_string(),
            },
            request,
        )
        .await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let allowed = NetworkPublishRequest {
            network_id: Some("agent-feed-mainnet".to_string()),
            compatibility: Some(ProtocolCompatibility::current()),
            feed_name: Some("workstation".to_string()),
            feed_id: Some("github:123:workstation".to_string()),
            visibility: Some(FeedVisibility::GithubOrg),
            github_org: Some("aberration-technology".to_string()),
            github_team: None,
            publisher: Some(PublisherIdentity::github(
                123,
                "mosure",
                Some("mosure".to_string()),
                Some("/avatar/github/123".to_string()),
            )),
            capsules: Vec::new(),
        };
        let response = network_publish_verified(
            state.clone(),
            VerifiedBearer {
                session: VerifiedSession {
                    github_user_id: 123,
                    scopes: ["read:org".to_string()].into_iter().collect(),
                    github_orgs: ["aberration-technology".to_string()].into_iter().collect(),
                    github_teams: BTreeSet::new(),
                },
                token: "test-session-token".to_string(),
            },
            allowed,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let public_snapshot = network_snapshot_value(&state.config, &state.snapshot);
        assert_eq!(public_snapshot["feeds"].as_array().expect("feeds").len(), 0);
        let filter = OrgRouteFilter::from_query(Some("all")).expect("filter parses");
        let org = GithubOrgName::parse("aberration-technology").expect("org parses");
        let org_feeds = state.snapshot.feeds_for_org(&org, None, &filter);
        assert_eq!(org_feeds.len(), 1);
        assert_eq!(org_feeds[0].visibility, "github_org");
    }

    #[tokio::test]
    async fn publish_edge_canary_accepts_capsule_and_snapshot_exposes_headline() {
        let state = Arc::new(HttpState {
            config: config(),
            snapshot: SnapshotStore::default(),
        });
        let feed_id = "github:123:workstation";
        let publisher = PublisherIdentity::github(
            123,
            "mosure",
            Some("mosure".to_string()),
            Some("/avatar/github/123".to_string()),
        );
        let story = CompiledStory {
            key: StoryKey {
                feed_id: Some(feed_id.to_string()),
                agent: "codex".to_string(),
                project_hash: Some("feed".to_string()),
                session_id: Some("canary-session".to_string()),
                turn_id: Some("canary-turn".to_string()),
                family: StoryFamily::Turn,
            },
            created_at: OffsetDateTime::now_utc(),
            family: StoryFamily::Turn,
            agent: "codex".to_string(),
            project: Some("feed".to_string()),
            headline: "feed publish path exposes delivery status".to_string(),
            deck: "operators can confirm local stories reached the public edge snapshot without a page refresh.".to_string(),
            lower_third: "codex · feed · score 91 · redacted".to_string(),
            chips: vec![
                "feed".to_string(),
                "codex".to_string(),
                "publish".to_string(),
                "edge".to_string(),
                "4 files".to_string(),
            ],
            severity: Severity::Notice,
            score: 91,
            context_score: 92,
            privacy: PrivacyClass::Redacted,
            evidence_event_ids: vec!["evt_canary_publish".to_string()],
        };
        let capsule = StoryCapsule::from_story(feed_id, 1, "local:codex", &story)
            .expect("story becomes capsule")
            .with_publisher(publisher.clone())
            .expect("publisher attaches");
        let signed = Signed::sign_capsule_with_secret(capsule, "github:123", "test-session-token")
            .expect("capsule signs with session secret");
        let request = NetworkPublishRequest {
            network_id: Some("agent-feed-mainnet".to_string()),
            compatibility: Some(ProtocolCompatibility::current()),
            feed_name: Some("workstation".to_string()),
            feed_id: Some(feed_id.to_string()),
            visibility: Some(FeedVisibility::Public),
            github_org: None,
            github_team: None,
            publisher: Some(publisher),
            capsules: vec![signed],
        };

        let response = network_publish_verified(
            state.clone(),
            VerifiedBearer {
                session: VerifiedSession {
                    github_user_id: 123,
                    scopes: BTreeSet::new(),
                    github_orgs: BTreeSet::new(),
                    github_teams: BTreeSet::new(),
                },
                token: "test-session-token".to_string(),
            },
            request,
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let snapshot = network_snapshot_value(&state.config, &state.snapshot);
        let feeds = snapshot["feeds"].as_array().expect("feeds");
        let headlines = snapshot["headlines"].as_array().expect("headlines");
        assert_eq!(feeds.len(), 1);
        assert_eq!(headlines.len(), 1);
        assert_eq!(feeds[0]["feed_id"], serde_json::json!(feed_id));
        assert_eq!(headlines[0]["feed_id"], serde_json::json!(feed_id));
        assert!(headlines[0]["created_at"].is_string());
        assert_eq!(headlines[0]["publisher_login"], serde_json::json!("mosure"));
        assert_eq!(
            headlines[0]["publisher_github_user_id"],
            serde_json::json!(123)
        );
        let headline = headlines[0]["headline"].as_str().expect("headline");
        let deck = headlines[0]["deck"].as_str().expect("deck");
        assert!(headline.contains("publish") || deck.contains("publish"));
        let display = serde_json::to_string(&snapshot).expect("snapshot serializes");
        assert!(!display.contains("raw prompt"));
        assert!(!display.contains("command output"));
        assert!(!display.contains("test-session-token"));
    }

    #[test]
    fn github_route_headline_filter_uses_stable_id_and_stream_label() {
        let route = RemoteUserRoute::parse("/mosure/workstation", None).expect("route parses");
        let matching = RemoteHeadlineView {
            feed_id: "github:123:workstation".to_string(),
            feed_label: "workstation".to_string(),
            compatibility: ProtocolCompatibility::current(),
            visibility: "public".to_string(),
            access: FeedAccessPolicy::public(),
            created_at: Some(OffsetDateTime::now_utc()),
            publisher_github_user_id: Some(123),
            publisher_login: "old-login".to_string(),
            publisher_display_name: None,
            publisher_avatar: None,
            verified: true,
            headline: "codex finished release pass".to_string(),
            deck: "settled story capsule reached the edge.".to_string(),
            lower_third: "@mosure / workstation".to_string(),
            chips: vec!["agent_reel".to_string(), "verified".to_string()],
            score: 84,
            image: None,
        };
        let wrong_stream = RemoteHeadlineView {
            feed_label: "release".to_string(),
            ..matching.clone()
        };

        assert!(headline_matches_github_route(
            &matching, 123, "mosure", &route
        ));
        assert!(!headline_matches_github_route(
            &wrong_stream,
            123,
            "mosure",
            &route
        ));
        assert!(!headline_matches_github_route(
            &matching, 456, "mosure", &route
        ));

        let project_route = RemoteUserRoute::parse(
            "/mosure/workstation",
            Some("projects=agent-reel&min_score=80"),
        )
        .expect("project route parses");
        let wrong_project = RemoteHeadlineView {
            chips: vec!["burn_p2p".to_string(), "verified".to_string()],
            ..matching.clone()
        };
        let low_score = RemoteHeadlineView {
            score: 70,
            ..matching.clone()
        };
        assert!(headline_matches_github_route(
            &matching,
            123,
            "mosure",
            &project_route
        ));
        assert!(!headline_matches_github_route(
            &wrong_project,
            123,
            "mosure",
            &project_route
        ));
        assert!(!headline_matches_github_route(
            &low_score,
            123,
            "mosure",
            &project_route
        ));
    }

    #[test]
    fn edge_org_resolver_returns_signed_org_ticket() {
        let profile = profile();
        let mut directory = DirectoryStore::new();
        directory
            .publish(org_entry(&profile, "feed-org-a", "peer-a", None))
            .expect("first org record publishes");
        let mut second = org_entry(&profile, "feed-org-b", "peer-b", None);
        second.feed_label = "release".to_string();
        second = second.sign("peer-b").expect("second resigns");
        directory
            .publish(second)
            .expect("second org record publishes");
        let edge = EdgeResolver::new(
            config(),
            StaticGithubResolver::new().with_profile(profile),
            directory,
        );
        let org = GithubOrgName::parse("aberration-technology").expect("org parses");
        let filter = OrgRouteFilter::from_query(Some("all")).expect("filter parses");

        let ticket = edge
            .resolve_github_org(&org, None, &filter)
            .expect("org resolves");

        assert_eq!(ticket.org, org);
        assert_eq!(ticket.candidate_feeds.len(), 2);
        assert!(ticket.verify_signature().expect("ticket verifies"));
        assert!(!ticket.rendezvous_namespaces[0].contains("aberration"));
    }

    #[test]
    fn edge_org_policy_requires_team_membership_when_configured() {
        let profile = profile();
        let org = GithubOrgName::parse("aberration-technology").expect("org parses");
        let release = GithubTeamSlug::parse("release").expect("team parses");
        let config = EdgeConfig {
            org_policy: OrgDeploymentPolicy {
                required_org: Some(org.clone()),
                required_teams: vec![release.clone()],
            },
            ..config()
        };
        let directory = DirectoryStore::new();
        let github = StaticGithubResolver::new().with_profile(profile.clone());
        let allowed = StaticGithubAccessResolver::new().with_member(
            org.clone(),
            profile.clone(),
            [release.clone()],
        );
        let denied = StaticGithubAccessResolver::new().with_member(org, profile, []);

        let allowed_edge = EdgeResolver::new_with_access(
            config.clone(),
            github.clone(),
            allowed,
            directory.clone(),
        );
        let denied_edge = EdgeResolver::new_with_access(config, github, denied, directory);
        let route = RemoteUserRoute::parse("/mosure", Some("all")).expect("route parses");

        assert!(allowed_edge.resolve_github_user(&route).is_ok());
        assert!(matches!(
            denied_edge.resolve_github_user(&route),
            Err(EdgeError::Github(GithubResolveError::Forbidden(_)))
        ));
    }
}
