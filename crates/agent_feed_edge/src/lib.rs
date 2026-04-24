use agent_feed_directory::{
    DirectoryError, DirectoryStore, GithubDiscoveryTicket, GithubProfileView, OrgDiscoveryTicket,
    OrgRouteFilter, RemoteUserRoute, SignedBrowserSeed,
};
use agent_feed_identity::{GithubLogin, GithubOrgName, GithubTeamSlug};
use agent_feed_identity_github::{
    AllowAllGithubAccess, GithubAccessResolver, GithubResolveError, GithubResolver,
    GithubUserResponse, StaticGithubResolver,
};
use agent_feed_p2p_proto::{
    ProtocolCompatibility, Signature, github_org_provider_key, github_org_topic,
    github_provider_key, github_team_provider_key, github_team_topic, github_user_topic,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};

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
            edge_domain: "https://edge.feed.aberration.technology".to_string(),
            browser_app_base_url: "https://feed.aberration.technology".to_string(),
            github_callback_url: "https://feed.aberration.technology/callback/github".to_string(),
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
    pub browser_seed_url: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveFeedView {
    pub feed_id: String,
    pub label: String,
    pub compatibility: ProtocolCompatibility,
    pub visibility: String,
    pub publisher_login: String,
    pub publisher_display_name: Option<String>,
    pub publisher_avatar: Option<String>,
    pub publisher_verified: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
}

impl From<&GithubDiscoveryTicket> for ResolveGithubResponse {
    fn from(ticket: &GithubDiscoveryTicket) -> Self {
        Self {
            state: "resolved".to_string(),
            network_id: ticket.network_id.clone(),
            compatibility: ticket.compatibility.clone(),
            requested_login: ticket.requested_login.to_string(),
            github_user_id: ticket.resolved_github_id.get(),
            profile: ticket.profile.clone(),
            feeds: ticket
                .candidate_feeds
                .iter()
                .map(|feed| ResolveFeedView {
                    feed_id: feed.feed_id.clone(),
                    label: feed.feed_label.clone(),
                    compatibility: feed.compatibility.clone(),
                    visibility: format!("{:?}", feed.visibility).to_ascii_lowercase(),
                    publisher_login: feed.owner.current_login.clone(),
                    publisher_display_name: feed.owner.display_name.clone(),
                    publisher_avatar: feed.owner.avatar.clone(),
                    publisher_verified: true,
                    last_seen_at: feed.last_seen_at,
                })
                .collect(),
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
    pub browser_seed_url: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub signature: String,
}

impl From<&OrgDiscoveryTicket> for ResolveOrgResponse {
    fn from(ticket: &OrgDiscoveryTicket) -> Self {
        Self {
            state: "resolved".to_string(),
            network_id: ticket.network_id.clone(),
            compatibility: ticket.compatibility.clone(),
            org: ticket.org.to_string(),
            team: ticket.team.as_ref().map(ToString::to_string),
            feeds: ticket
                .candidate_feeds
                .iter()
                .map(|feed| ResolveFeedView {
                    feed_id: feed.feed_id.clone(),
                    label: feed.feed_label.clone(),
                    compatibility: feed.compatibility.clone(),
                    visibility: format!("{:?}", feed.visibility).to_ascii_lowercase(),
                    publisher_login: feed.owner.current_login.clone(),
                    publisher_display_name: feed.owner.display_name.clone(),
                    publisher_avatar: feed.owner.avatar.clone(),
                    publisher_verified: true,
                    last_seen_at: feed.last_seen_at,
                })
                .collect(),
            browser_seed_url: "/browser-seed".to_string(),
            expires_at: ticket.expires_at,
            signature: ticket.signature.digest.clone(),
        }
    }
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
}

pub async fn serve_http(config: EdgeServerConfig) -> Result<(), EdgeServeError> {
    if config.fabric.enabled {
        spawn_fabric_listeners(config.edge.clone(), config.fabric.clone()).await?;
    }
    let state = Arc::new(HttpState {
        config: config.edge,
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
        .route("/browser-seed", get(browser_seed))
        .route("/network/snapshot", get(network_snapshot))
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
    serde_json::json!({
        "product": "feed",
        "protocol": "agent-feed.edge/1",
        "protocol_version": agent_feed_p2p_proto::AGENT_FEED_PROTOCOL_VERSION,
        "model_version": agent_feed_p2p_proto::AGENT_FEED_MODEL_VERSION,
        "min_model_version": agent_feed_p2p_proto::AGENT_FEED_MIN_MODEL_VERSION,
        "release_version": agent_feed_p2p_proto::AGENT_FEED_RELEASE_VERSION,
        "transport": transport,
        "network_id": edge.network_id,
        "edge": edge.edge_domain,
        "state": "ready"
    })
    .to_string()
        + "\n"
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
    Json(serde_json::json!({
        "state": "ready",
        "product": "feed",
        "protocol_version": agent_feed_p2p_proto::AGENT_FEED_PROTOCOL_VERSION,
        "model_version": agent_feed_p2p_proto::AGENT_FEED_MODEL_VERSION,
        "min_model_version": agent_feed_p2p_proto::AGENT_FEED_MIN_MODEL_VERSION,
        "release_version": agent_feed_p2p_proto::AGENT_FEED_RELEASE_VERSION,
    }))
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
    };
    let Ok(encoded_state) = encode_oauth_state(&payload) else {
        return edge_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "github auth state failed",
        );
    };
    let callback_url = state.config.github_callback_url();
    let scope = query
        .get("scope")
        .cloned()
        .unwrap_or_else(|| default_github_scope(&state.config.org_policy));
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
    let profile = match fetch_github_profile(&token) {
        Ok(profile) => profile,
        Err(message) => return edge_error(StatusCode::BAD_GATEWAY, message),
    };
    if let Err(message) = authorize_github_token_policy(&token, &profile, &state.config.org_policy)
    {
        return edge_error(StatusCode::FORBIDDEN, message);
    }
    let session = format!(
        "feed.{}.{}",
        profile.id.get(),
        OffsetDateTime::now_utc().unix_timestamp()
    );
    let final_url = if payload.client.ends_with("-cli") {
        payload.redirect_uri.unwrap_or_default()
    } else {
        format!(
            "{}/callback/github",
            origin_of_url(&payload.return_to)
                .unwrap_or_else(|| state.config.browser_app_base_url.clone())
        )
    };
    let expires_at = OffsetDateTime::now_utc() + Duration::days(7);
    let github_user_id = profile.id.get().to_string();
    let expires_at = expires_at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let redirect_url = format!(
        "{}?{}",
        final_url,
        encode_query(&[
            ("state", payload.state.as_str()),
            ("github_user_id", github_user_id.as_str()),
            ("login", profile.login.as_str()),
            ("name", profile.name.as_deref().unwrap_or("")),
            ("avatar_url", profile.avatar_url.as_deref().unwrap_or("")),
            ("session", session.as_str()),
            ("expires_at", expires_at.as_str()),
            ("return_to", payload.return_to.as_str()),
        ])
    );
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
        Ok(ticket) => Json(ResolveGithubResponse::from(&ticket)).into_response(),
        Err(EdgeError::Github(GithubResolveError::NotFound(_))) => {
            edge_error(StatusCode::NOT_FOUND, "github user not found")
        }
        Err(EdgeError::Github(GithubResolveError::RateLimited)) => edge_error(
            StatusCode::TOO_MANY_REQUESTS,
            "github resolver rate limited",
        ),
        Err(err) => edge_error(StatusCode::BAD_GATEWAY, err.to_string()),
    }
}

async fn resolve_github_org(
    State(state): State<Arc<HttpState>>,
    Path(org): Path<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    resolve_org_like(state, org, None, query)
}

async fn resolve_github_team(
    State(state): State<Arc<HttpState>>,
    Path((org, team)): Path<(String, String)>,
    Query(query): Query<BTreeMap<String, String>>,
) -> impl IntoResponse {
    resolve_org_like(state, org, Some(team), query)
}

fn resolve_org_like(
    state: Arc<HttpState>,
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
        Ok(ticket) => Json(ResolveOrgResponse::from(&ticket)).into_response(),
        Err(err) => edge_error(StatusCode::BAD_GATEWAY, err.to_string()),
    }
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

async fn network_snapshot(State(state): State<Arc<HttpState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "product": "feed",
        "network_id": state.config.network_id,
        "compatibility": ProtocolCompatibility::current(),
        "edge_base_url": state.config.edge_domain,
        "browser_app_base_url": state.config.browser_app_base_url,
        "bootstrap_peers": state.config.bootstrap_peers,
    }))
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GithubOauthState {
    client: String,
    state: String,
    redirect_uri: Option<String>,
    return_to: String,
}

#[derive(Debug, Deserialize)]
struct GithubTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
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
) -> Result<String, String> {
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
    response
        .access_token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "github token response missing access token".to_string())
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
    use agent_feed_directory::{FeedDirectoryEntry, GithubPrincipal};
    use agent_feed_identity::{GithubOrgName, GithubTeamSlug, GithubUserId};
    use agent_feed_identity_github::{GithubProfile, StaticGithubAccessResolver};
    use agent_feed_p2p_proto::FeedVisibility;

    fn profile() -> GithubProfile {
        GithubProfile {
            id: GithubUserId::new(123),
            login: GithubLogin::parse("mosure").expect("login parses"),
            name: Some("mosure".to_string()),
            avatar_url: Some("/avatar/github/123".to_string()),
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
        let response = ResolveGithubResponse::from(&ticket);

        assert_eq!(response.state, "resolved");
        assert_eq!(response.requested_login, "mosure");
        assert_eq!(response.github_user_id, 123);
        assert_eq!(response.compatibility.protocol_version, 1);
        assert_eq!(response.compatibility.model_version, 1);
        assert_eq!(response.browser_seed_url, "/browser-seed");
        assert_eq!(response.feeds[0].publisher_login, "mosure");
        assert_eq!(response.feeds[0].compatibility.protocol_version, 1);
        assert_eq!(
            response.feeds[0].publisher_avatar.as_deref(),
            Some("/avatar/github/123")
        );
        assert!(response.feeds[0].publisher_verified);
    }

    #[test]
    fn github_oauth_callback_uses_feed_host() {
        let config = config();

        assert_eq!(
            config.github_callback_url(),
            "https://feed.aberration.technology/callback/github"
        );
        assert_ne!(
            config.github_callback_url(),
            "https://edge.feed.aberration.technology/callback/github"
        );
    }

    #[test]
    fn fabric_probe_payload_is_display_safe() {
        let payload = fabric_probe_payload(&config(), "tcp");

        assert!(payload.contains("\"product\":\"feed\""));
        assert!(payload.contains("\"protocol\":\"agent-feed.edge/1\""));
        assert!(payload.contains("\"protocol_version\":1"));
        assert!(payload.contains("\"model_version\":1"));
        assert!(payload.contains("\"transport\":\"tcp\""));
        assert!(payload.contains("\"state\":\"ready\""));
        assert!(!payload.contains("secret"));
        assert!(!payload.contains("token"));
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
