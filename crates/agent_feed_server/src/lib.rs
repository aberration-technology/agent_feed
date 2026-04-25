use agent_feed_core::{AgentEvent, Bulletin, SourceKind};
use agent_feed_ingest::normalize_value;
use agent_feed_metrics::Metrics;
use agent_feed_redaction::Redactor;
use agent_feed_reel::{ReelBuffer, ReelSnapshot};
use agent_feed_security::{
    SecurityConfig, SecurityError, requires_display_token, token_matches, validate_bind,
};
use agent_feed_store::InMemoryStore;
use agent_feed_story::StoryCompiler;
use agent_feed_ui::UiConfig;
use agent_feed_views::{
    AdaptersView, AgentsView, BulletinsView, EventsView, HealthView, IngestView, SessionsView,
    SseBulletin, StatusView,
};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::time as tokio_time;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{StreamExt, once};
use tower_http::trace::TraceLayer;

#[derive(Clone, Debug, Default)]
pub struct ServerConfig {
    pub security: SecurityConfig,
    pub p2p_enabled: bool,
}

const STORY_FLUSH_INTERVAL: Duration = Duration::from_secs(15);

impl ServerConfig {
    #[must_use]
    pub fn bind(&self) -> SocketAddr {
        self.security.bind
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Security(#[from] SecurityError),
    #[error("server io failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
struct AppState {
    bind: SocketAddr,
    p2p_enabled: bool,
    security: SecurityConfig,
    reel: Mutex<ReelBuffer>,
    store: Mutex<InMemoryStore>,
    tx: broadcast::Sender<Bulletin>,
    redactor: Redactor,
    story: Mutex<StoryCompiler>,
    metrics: Metrics,
}

pub async fn serve(config: ServerConfig) -> Result<(), ServerError> {
    serve_with_ready(config, |_| {}).await
}

pub async fn serve_with_ready<F>(config: ServerConfig, ready: F) -> Result<(), ServerError>
where
    F: FnOnce(SocketAddr) + Send + 'static,
{
    validate_bind(&config.security)?;
    let bind = config.bind();
    let listener = TcpListener::bind(bind).await?;
    let app = app_with_server_config(config.clone());
    tracing::info!(%bind, "agent_feed serving");
    ready(bind);
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn app(bind: SocketAddr) -> Router {
    app_with_config(bind, false)
}

pub fn app_with_config(bind: SocketAddr, p2p_enabled: bool) -> Router {
    app_with_server_config(ServerConfig {
        security: SecurityConfig {
            bind,
            ..SecurityConfig::default()
        },
        p2p_enabled,
    })
}

pub fn app_with_server_config(config: ServerConfig) -> Router {
    let (tx, _) = broadcast::channel(128);
    let state = Arc::new(AppState {
        bind: config.security.bind,
        p2p_enabled: config.p2p_enabled,
        security: config.security,
        reel: Mutex::new(ReelBuffer::default()),
        store: Mutex::new(InMemoryStore::default()),
        tx,
        redactor: Redactor::default(),
        story: Mutex::new(StoryCompiler::default()),
        metrics: Metrics::default(),
    });
    spawn_story_flush_loop(state.clone());

    Router::new()
        .route("/", get(root_index))
        .route("/reel", get(reel_index))
        .route("/reel/{view}", get(reel_view))
        .route("/favicon.svg", get(favicon_svg))
        .route("/network", get(network_index))
        .route("/callback/github", get(github_callback_shell))
        .route("/events.sse", get(events_sse))
        .route("/api/reel/snapshot", get(reel_snapshot))
        .route("/api/bulletins", get(bulletins))
        .route("/api/events", get(events))
        .route("/api/agents", get(agents))
        .route("/api/sessions", get(sessions))
        .route("/api/adapters", get(adapters))
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/ingest/codex/jsonl", post(ingest_codex))
        .route("/ingest/codex/hook", post(ingest_codex))
        .route("/ingest/claude/stream-json", post(ingest_claude))
        .route("/ingest/claude/hook", post(ingest_claude))
        .route("/ingest/mcp", post(ingest_mcp))
        .route("/ingest/otel", post(ingest_otel))
        .route("/ingest/generic", post(ingest_generic))
        .route("/{remote}", get(remote_user_shell))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            display_token_auth,
        ))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

fn spawn_story_flush_loop(state: Arc<AppState>) {
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::debug!("story flush loop skipped outside tokio runtime");
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio_time::interval(STORY_FLUSH_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(err) = flush_story_windows(state.clone()).await {
                tracing::error!(error = ?err, "story flush loop failed");
            }
        }
    });
}

async fn display_token_auth(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !requires_display_token(&state.security) || display_auth_is_public(request.uri().path()) {
        return next.run(request).await;
    }
    let Some(expected) = state.security.display_token.as_deref() else {
        return next.run(request).await;
    };
    if request_display_token(&request).is_some_and(|actual| token_matches(expected, &actual)) {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"agent-feed\"")],
        "display token required",
    )
        .into_response()
}

fn display_auth_is_public(path: &str) -> bool {
    matches!(path, "/api/health" | "/favicon.svg")
}

fn request_display_token(request: &Request<Body>) -> Option<String> {
    request
        .headers()
        .get("x-agent-feed-display-token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().strip_prefix("Bearer "))
                .map(str::trim)
                .map(str::to_string)
        })
        .or_else(|| query_param(request.uri().query().unwrap_or_default(), "display_token"))
        .or_else(|| query_param(request.uri().query().unwrap_or_default(), "token"))
}

fn query_param(query: &str, needle: &str) -> Option<String> {
    query.split('&').find_map(|part| {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        (key == needle && !value.is_empty()).then(|| value.to_string())
    })
}

async fn root_index(State(state): State<Arc<AppState>>) -> Html<String> {
    let view = if state.p2p_enabled { "remote" } else { "stage" };
    Html(render_ui(Some(view), state.p2p_enabled))
}

async fn reel_index(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(render_ui(Some("stage"), state.p2p_enabled))
}

async fn reel_view(State(state): State<Arc<AppState>>, Path(view): Path<String>) -> Html<String> {
    Html(render_ui(Some(&view), state.p2p_enabled))
}

async fn network_index(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(render_ui(Some("network"), state.p2p_enabled))
}

async fn github_callback_shell(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(render_ui(Some("network"), state.p2p_enabled))
}

async fn favicon_svg() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")],
        agent_feed_ui::FAVICON_SVG,
    )
}

async fn remote_user_shell(
    State(state): State<Arc<AppState>>,
    Path(remote): Path<String>,
) -> Response {
    match agent_feed_identity::GithubLogin::parse(remote.as_str()) {
        Ok(_) => Html(render_ui(Some("remote"), state.p2p_enabled)).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn render_ui(view: Option<&str>, p2p_enabled: bool) -> String {
    agent_feed_ui::render_index_with_config(
        view,
        &UiConfig {
            p2p_enabled,
            revision: option_env!("GITHUB_SHA")
                .map(short_revision)
                .or_else(|| option_env!("VERGEN_GIT_SHA").map(short_revision)),
        },
    )
}

fn short_revision(value: &str) -> String {
    value.chars().take(12).collect()
}

async fn events_sse(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    tracing::debug!("sse client connected");
    let hello = once(Ok(Event::default().comment("agent_feed connected")));
    let bulletins =
        BroadcastStream::new(state.tx.subscribe()).filter_map(|message| match message {
            Ok(bulletin) => Event::default()
                .event("bulletin")
                .json_data(SseBulletin {
                    message_type: "bulletin",
                    bulletin,
                })
                .ok()
                .map(Ok),
            Err(err) => {
                tracing::warn!(error = %err, "sse bulletin stream lagged");
                None
            }
        });

    Sse::new(hello.chain(bulletins)).keep_alive(KeepAlive::new().text("agent_feed keepalive"))
}

async fn reel_snapshot(State(state): State<Arc<AppState>>) -> Result<Json<ReelSnapshot>, AppError> {
    let snapshot = state
        .reel
        .lock()
        .map_err(|_| AppError::StatePoisoned)?
        .snapshot();
    Ok(Json(snapshot))
}

async fn bulletins(State(state): State<Arc<AppState>>) -> Result<Json<BulletinsView>, AppError> {
    let snapshot = state
        .reel
        .lock()
        .map_err(|_| AppError::StatePoisoned)?
        .snapshot();
    Ok(Json(BulletinsView {
        bulletins: snapshot.bulletins,
    }))
}

async fn events(State(state): State<Arc<AppState>>) -> Result<Json<EventsView>, AppError> {
    let events = state
        .store
        .lock()
        .map_err(|_| AppError::StatePoisoned)?
        .events();
    Ok(Json(EventsView { events }))
}

async fn agents(State(state): State<Arc<AppState>>) -> Result<Json<AgentsView>, AppError> {
    let events = state
        .store
        .lock()
        .map_err(|_| AppError::StatePoisoned)?
        .events();
    let agents = events
        .into_iter()
        .map(|event| event.agent)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Json(AgentsView { agents }))
}

async fn sessions(State(state): State<Arc<AppState>>) -> Result<Json<SessionsView>, AppError> {
    let events = state
        .store
        .lock()
        .map_err(|_| AppError::StatePoisoned)?
        .events();
    let sessions = events
        .into_iter()
        .filter_map(|event| event.session_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Json(SessionsView { sessions }))
}

async fn adapters(State(state): State<Arc<AppState>>) -> Result<Json<AdaptersView>, AppError> {
    let events = state
        .store
        .lock()
        .map_err(|_| AppError::StatePoisoned)?
        .events();
    let adapters = events
        .into_iter()
        .map(|event| event.adapter)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Json(AdaptersView { adapters }))
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthView> {
    let metrics = state.metrics.snapshot();
    Json(HealthView {
        status: "ok",
        bind: state.bind.to_string(),
        ingested_events: metrics.ingested_events,
        emitted_bulletins: metrics.emitted_bulletins,
        dropped_events: metrics.dropped_events,
    })
}

async fn status(State(state): State<Arc<AppState>>) -> Result<Json<StatusView>, AppError> {
    let metrics = state.metrics.snapshot();
    let events = state
        .store
        .lock()
        .map_err(|_| AppError::StatePoisoned)?
        .events();
    let snapshot = state
        .reel
        .lock()
        .map_err(|_| AppError::StatePoisoned)?
        .snapshot();
    let mut sources = BTreeMap::<(String, String, String), SourceStatus>::new();
    for event in &events {
        let key = (
            event.source.as_str().to_string(),
            event.agent.clone(),
            event.adapter.clone(),
        );
        let occurred_at = event.occurred_at.unwrap_or(event.received_at);
        sources
            .entry(key)
            .and_modify(|status| status.observe(event, occurred_at))
            .or_insert_with(|| SourceStatus::new(event, occurred_at));
    }
    let last_event = events
        .iter()
        .max_by_key(|event| event.occurred_at.unwrap_or(event.received_at));
    let captured_sources = sources
        .into_iter()
        .map(
            |((source, agent, adapter), status)| agent_feed_views::CapturedSourceView {
                source,
                agent,
                adapter,
                events: status.events,
                sessions: status.sessions.len(),
                last_event_kind: status.last_event_kind,
                last_event_at: status.last_event_at,
            },
        )
        .collect();
    Ok(Json(StatusView {
        status: "ok",
        bind: state.bind.to_string(),
        p2p_enabled: state.p2p_enabled,
        ingested_events: metrics.ingested_events,
        emitted_bulletins: metrics.emitted_bulletins,
        dropped_events: metrics.dropped_events,
        stored_events: events.len(),
        stored_bulletins: snapshot.bulletins.len(),
        captured_sources,
        last_event_kind: last_event.map(|event| event.kind.as_str().to_string()),
        last_event_at: last_event.map(|event| event.occurred_at.unwrap_or(event.received_at)),
        last_bulletin_at: snapshot.active.map(|bulletin| bulletin.created_at),
    }))
}

#[derive(Debug)]
struct SourceStatus {
    events: usize,
    sessions: BTreeSet<String>,
    last_event_kind: String,
    last_event_at: time::OffsetDateTime,
}

impl SourceStatus {
    fn new(event: &AgentEvent, occurred_at: time::OffsetDateTime) -> Self {
        let mut sessions = BTreeSet::new();
        if let Some(session_id) = &event.session_id {
            sessions.insert(session_id.clone());
        }
        Self {
            events: 1,
            sessions,
            last_event_kind: event.kind.as_str().to_string(),
            last_event_at: occurred_at,
        }
    }

    fn observe(&mut self, event: &AgentEvent, occurred_at: time::OffsetDateTime) {
        self.events += 1;
        if let Some(session_id) = &event.session_id {
            self.sessions.insert(session_id.clone());
        }
        if occurred_at >= self.last_event_at {
            self.last_event_kind = event.kind.as_str().to_string();
            self.last_event_at = occurred_at;
        }
    }
}

async fn ingest_generic(
    State(state): State<Arc<AppState>>,
    Json(value): Json<Value>,
) -> Result<Json<IngestView>, AppError> {
    ingest_value(state, value, SourceKind::Generic).await
}

async fn ingest_codex(
    State(state): State<Arc<AppState>>,
    Json(value): Json<Value>,
) -> Result<Json<IngestView>, AppError> {
    ingest_agent_value(state, value, SourceKind::Codex).await
}

async fn ingest_claude(
    State(state): State<Arc<AppState>>,
    Json(value): Json<Value>,
) -> Result<Json<IngestView>, AppError> {
    ingest_agent_value(state, value, SourceKind::Claude).await
}

async fn ingest_mcp(
    State(state): State<Arc<AppState>>,
    Json(value): Json<Value>,
) -> Result<Json<IngestView>, AppError> {
    ingest_value(state, value, SourceKind::Mcp).await
}

async fn ingest_otel(
    State(state): State<Arc<AppState>>,
    Json(value): Json<Value>,
) -> Result<Json<IngestView>, AppError> {
    ingest_value(state, value, SourceKind::Otel).await
}

async fn ingest_value(
    state: Arc<AppState>,
    value: Value,
    source: SourceKind,
) -> Result<Json<IngestView>, AppError> {
    let event = normalize_value(value, source).map_err(|err| {
        tracing::warn!(%source, error = %err, "ingest normalization failed");
        AppError::BadInput(err.to_string())
    })?;
    ingest_event(state, event).await
}

async fn ingest_agent_value(
    state: Arc<AppState>,
    value: Value,
    source: SourceKind,
) -> Result<Json<IngestView>, AppError> {
    let event = match source {
        SourceKind::Codex => {
            agent_feed_adapters::codex::normalize_exec_json(value).map_err(|err| {
                tracing::warn!(%source, error = %err, "adapter normalization failed");
                AppError::BadInput(err.to_string())
            })?
        }
        SourceKind::Claude => {
            agent_feed_adapters::claude::normalize_stream_json(value).map_err(|err| {
                tracing::warn!(%source, error = %err, "adapter normalization failed");
                AppError::BadInput(err.to_string())
            })?
        }
        _ => unreachable!("ingest_agent_value only handles first-party agent adapters"),
    };
    ingest_event(state, event).await
}

async fn ingest_event(
    state: Arc<AppState>,
    event: AgentEvent,
) -> Result<Json<IngestView>, AppError> {
    let event = state.redactor.redact_event(event);
    let event_id = event.id.to_string();
    let event_kind = event.kind.as_str();
    let session_id = event.session_id.clone();
    let project = event.project.clone();
    let stories = state
        .story
        .lock()
        .map_err(|_| {
            tracing::error!("story compiler lock poisoned during ingest");
            AppError::StatePoisoned
        })?
        .ingest(event.clone());
    let bulletins = stories
        .iter()
        .map(agent_feed_story::CompiledStory::to_bulletin)
        .collect::<Vec<_>>();
    let response = ingest_response(&event, &bulletins);

    state
        .store
        .lock()
        .map_err(|_| {
            tracing::error!("event store lock poisoned during ingest");
            AppError::StatePoisoned
        })?
        .push(event);

    state.metrics.record_ingested();
    emit_bulletins(&state, bulletins, "ingest")?;

    tracing::info!(
        %event_id,
        event_kind,
        session_id,
        project,
        emitted_bulletins = response.bulletin_ids.len(),
        "event ingested"
    );

    Ok(Json(response))
}

async fn flush_story_windows(state: Arc<AppState>) -> Result<usize, AppError> {
    let stories = state
        .story
        .lock()
        .map_err(|_| {
            tracing::error!("story compiler lock poisoned during flush");
            AppError::StatePoisoned
        })?
        .flush();
    if stories.is_empty() {
        return Ok(0);
    }
    let bulletins = stories
        .iter()
        .map(agent_feed_story::CompiledStory::to_bulletin)
        .collect::<Vec<_>>();
    let count = bulletins.len();
    emit_bulletins(&state, bulletins, "flush")?;
    tracing::info!(
        stories = stories.len(),
        bulletins = count,
        "story windows flushed"
    );
    Ok(count)
}

fn emit_bulletins(
    state: &Arc<AppState>,
    bulletins: Vec<Bulletin>,
    origin: &'static str,
) -> Result<(), AppError> {
    for bulletin in bulletins {
        let bulletin_id = bulletin.id.to_string();
        let headline = bulletin.headline.clone();
        state
            .reel
            .lock()
            .map_err(|_| {
                tracing::error!("reel buffer lock poisoned during bulletin emit");
                AppError::StatePoisoned
            })?
            .push(bulletin.clone());
        state.metrics.record_emitted();
        match state.tx.send(bulletin) {
            Ok(receivers) => {
                tracing::debug!(
                    %bulletin_id,
                    %origin,
                    receivers,
                    "story bulletin sent to sse subscribers"
                );
            }
            Err(err) => {
                tracing::debug!(
                    %bulletin_id,
                    %origin,
                    error = %err,
                    "story bulletin stored without sse subscribers"
                );
            }
        }
        tracing::info!(%bulletin_id, %origin, %headline, "story bulletin emitted");
    }
    Ok(())
}

fn ingest_response(event: &AgentEvent, bulletins: &[Bulletin]) -> IngestView {
    let bulletin_ids = bulletins
        .iter()
        .map(|bulletin| bulletin.id.to_string())
        .collect::<Vec<_>>();
    IngestView {
        accepted: true,
        event_id: event.id.to_string(),
        bulletin_id: bulletin_ids.first().cloned(),
        bulletin_ids,
        received_at: event.received_at,
    }
}

#[derive(Debug)]
enum AppError {
    BadInput(String),
    StatePoisoned,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::BadInput(message) => {
                tracing::warn!(error = %message, "request rejected");
                (StatusCode::BAD_REQUEST, message).into_response()
            }
            Self::StatePoisoned => {
                tracing::error!("request failed because server state was poisoned");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "agent_feed state lock poisoned",
                )
                    .into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_core::{EventKind, PrivacyClass, Severity};

    fn test_state() -> Arc<AppState> {
        test_state_with_p2p(false)
    }

    fn test_state_with_p2p(p2p_enabled: bool) -> Arc<AppState> {
        let (tx, _) = broadcast::channel(16);
        Arc::new(AppState {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            p2p_enabled,
            security: SecurityConfig {
                bind: SocketAddr::from(([127, 0, 0, 1], 0)),
                ..SecurityConfig::default()
            },
            reel: Mutex::new(ReelBuffer::default()),
            store: Mutex::new(InMemoryStore::default()),
            tx,
            redactor: Redactor::default(),
            story: Mutex::new(StoryCompiler::default()),
            metrics: Metrics::default(),
        })
    }

    #[tokio::test]
    async fn root_index_switches_to_remote_when_p2p_enabled() {
        let Html(local) = root_index(State(test_state_with_p2p(false))).await;
        let Html(remote) = root_index(State(test_state_with_p2p(true))).await;

        assert!(local.contains("<body data-view=\"stage\">"));
        assert!(local.contains("window.FEED_P2P_ENABLED = false;"));
        assert!(remote.contains("<body data-view=\"remote\">"));
        assert!(remote.contains("window.FEED_P2P_ENABLED = true;"));
    }

    #[tokio::test]
    async fn favicon_svg_route_is_embedded() {
        let response = favicon_svg().await.into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(
                &"image/svg+xml; charset=utf-8"
                    .parse()
                    .expect("valid header")
            )
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("favicon body");
        let body = std::str::from_utf8(&body).expect("favicon is utf-8");
        assert!(body.contains("fill=\"#d87c7c\""));
        assert!(body.contains("V30h-7v-8h7v-3"));
    }

    #[tokio::test]
    async fn ingest_redacts_before_store_and_story_compiler() {
        let state = test_state();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/tester".to_string());
        let mut event = AgentEvent::new(
            SourceKind::Codex,
            EventKind::FileChanged,
            "codex found sk-test_secret in patch",
        );
        event.agent = "codex".to_string();
        event.adapter = "codex.test".to_string();
        event.project = Some("agent_feed".to_string());
        event.session_id = Some("session".to_string());
        event.turn_id = Some("turn".to_string());
        event.summary = Some("patch mentioned ghp_privateToken and raw diff omitted.".to_string());
        event.command = Some("echo sk-test_secret".to_string());
        event.cwd = Some(format!("{home}/repo/.env"));
        event.files = vec![format!("{home}/repo/secrets/token.key")];
        event.score_hint = Some(88);
        event.severity = Severity::Notice;

        let Json(view) = ingest_event(state.clone(), event)
            .await
            .expect("event ingests");

        assert!(view.accepted);
        let events = state.store.lock().expect("store lock").events();
        assert_eq!(events.len(), 1);
        let stored = &events[0];
        assert_eq!(stored.privacy, PrivacyClass::Redacted);
        let stored_text = serde_json::to_string(stored).expect("stored event serializes");
        assert!(!stored_text.contains("sk-test_secret"));
        assert!(!stored_text.contains("ghp_privateToken"));
        assert!(!stored_text.contains("secrets/token.key"));
        assert_eq!(stored.cwd.as_deref(), Some("[sensitive-path]"));
        assert_eq!(stored.files, vec!["[sensitive-path]"]);
        assert!(
            state
                .reel
                .lock()
                .expect("reel lock")
                .snapshot()
                .bulletins
                .iter()
                .all(|bulletin| {
                    let text = serde_json::to_string(bulletin).expect("bulletin serializes");
                    !text.contains("sk-test_secret") && !text.contains("ghp_privateToken")
                })
        );
    }

    #[tokio::test]
    async fn codex_ingest_route_uses_first_party_adapter_normalization() {
        let state = test_state();
        let value = serde_json::json!({
            "type": "turn.completed",
            "agent": "codex",
            "title": "turn.completed",
            "cwd": "/tmp/agent-feed-test-workspace",
            "session_id": "session",
            "summary": "turn completed without raw output"
        });

        let Json(view) = ingest_agent_value(state.clone(), value, SourceKind::Codex)
            .await
            .expect("codex event ingests");

        assert!(view.accepted);
        let events = state.store.lock().expect("store lock").events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::TurnComplete);
        assert_eq!(events[0].adapter, "codex.exec-json");
    }

    #[tokio::test]
    async fn status_reports_capture_health_without_story() {
        let state = test_state();
        let mut event = AgentEvent::new(
            SourceKind::Codex,
            EventKind::AdapterHealth,
            "codex capture active",
        );
        event.agent = "codex".to_string();
        event.adapter = "codex.transcript".to_string();
        event.session_id = Some("session-1".to_string());
        event.score_hint = Some(5);

        let _ = ingest_event(state.clone(), event)
            .await
            .expect("event ingests");
        let Json(view) = status(State(state)).await.expect("status renders");

        assert_eq!(view.stored_events, 1);
        assert_eq!(view.stored_bulletins, 0);
        assert_eq!(view.captured_sources.len(), 1);
        assert_eq!(view.captured_sources[0].agent, "codex");
        assert_eq!(view.captured_sources[0].sessions, 1);
        assert_eq!(view.last_event_kind.as_deref(), Some("adapter.health"));
    }

    #[tokio::test]
    async fn flush_suppresses_command_burst_without_work_outcome() {
        let state = test_state();
        for _ in 0..4 {
            let mut event = AgentEvent::new(
                SourceKind::Codex,
                EventKind::ToolComplete,
                "codex command completed",
            );
            event.agent = "codex".to_string();
            event.adapter = "codex.exec-json".to_string();
            event.project = Some("agent_feed".to_string());
            event.session_id = Some("session".to_string());
            event.turn_id = Some("turn".to_string());
            event.summary = Some("exit 0. raw output omitted.".to_string());
            event.command =
                Some("gh run view 24941390598 --repo example/project --json status".to_string());
            event.score_hint = Some(48);

            let Json(view) = ingest_event(state.clone(), event)
                .await
                .expect("event ingests");
            assert!(view.bulletin_ids.is_empty());
        }

        let flushed = flush_story_windows(state.clone())
            .await
            .expect("story flushes");

        assert_eq!(flushed, 0);
        let snapshot = state.reel.lock().expect("reel lock").snapshot();
        assert!(snapshot.bulletins.is_empty());
        let display = serde_json::to_string(&snapshot).expect("snapshot serializes");
        assert!(!display.contains("gh run view"));
        assert!(!display.contains("--repo"));
    }
}
