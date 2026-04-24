use agent_reel_core::{AgentEvent, Bulletin, SourceKind};
use agent_reel_ingest::normalize_value;
use agent_reel_metrics::Metrics;
use agent_reel_redaction::Redactor;
use agent_reel_reel::{ReelBuffer, ReelSnapshot};
use agent_reel_security::{SecurityConfig, SecurityError, validate_bind};
use agent_reel_store::InMemoryStore;
use agent_reel_story::StoryCompiler;
use agent_reel_views::{
    AdaptersView, AgentsView, BulletinsView, EventsView, HealthView, IngestView, SessionsView,
    SseBulletin,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{StreamExt, once};
use tower_http::trace::TraceLayer;

#[derive(Clone, Debug, Default)]
pub struct ServerConfig {
    pub security: SecurityConfig,
}

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
    reel: Mutex<ReelBuffer>,
    store: Mutex<InMemoryStore>,
    tx: broadcast::Sender<Bulletin>,
    redactor: Redactor,
    story: Mutex<StoryCompiler>,
    metrics: Metrics,
}

pub async fn serve(config: ServerConfig) -> Result<(), ServerError> {
    validate_bind(&config.security)?;
    let bind = config.bind();
    let listener = TcpListener::bind(bind).await?;
    let app = app(bind);
    tracing::info!(%bind, "agent_reel serving");
    axum::serve(listener, app).await?;
    Ok(())
}

pub fn app(bind: SocketAddr) -> Router {
    let (tx, _) = broadcast::channel(128);
    let state = Arc::new(AppState {
        bind,
        reel: Mutex::new(ReelBuffer::default()),
        store: Mutex::new(InMemoryStore::default()),
        tx,
        redactor: Redactor::default(),
        story: Mutex::new(StoryCompiler::default()),
        metrics: Metrics::default(),
    });

    Router::new()
        .route("/reel", get(reel_index))
        .route("/reel/{view}", get(reel_view))
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
        .route("/ingest/codex/jsonl", post(ingest_codex))
        .route("/ingest/codex/hook", post(ingest_codex))
        .route("/ingest/claude/stream-json", post(ingest_claude))
        .route("/ingest/claude/hook", post(ingest_claude))
        .route("/ingest/mcp", post(ingest_mcp))
        .route("/ingest/otel", post(ingest_otel))
        .route("/ingest/generic", post(ingest_generic))
        .route("/{remote}", get(remote_user_shell))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

async fn reel_index() -> Html<String> {
    Html(agent_reel_ui::render_index(Some("stage")))
}

async fn reel_view(Path(view): Path<String>) -> Html<String> {
    Html(agent_reel_ui::render_index(Some(&view)))
}

async fn network_index() -> Html<String> {
    Html(agent_reel_ui::render_index(Some("network")))
}

async fn github_callback_shell() -> Html<String> {
    Html(agent_reel_ui::render_index(Some("network")))
}

async fn remote_user_shell(Path(remote): Path<String>) -> Response {
    match agent_reel_identity::GithubLogin::parse(remote.as_str()) {
        Ok(_) => Html(agent_reel_ui::render_index(Some("remote"))).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn events_sse(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    tracing::debug!("sse client connected");
    let hello = once(Ok(Event::default().comment("agent_reel connected")));
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

    Sse::new(hello.chain(bulletins)).keep_alive(KeepAlive::new().text("agent_reel keepalive"))
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
            agent_reel_adapters::codex::normalize_exec_json(value).map_err(|err| {
                tracing::warn!(%source, error = %err, "adapter normalization failed");
                AppError::BadInput(err.to_string())
            })?
        }
        SourceKind::Claude => {
            agent_reel_adapters::claude::normalize_stream_json(value).map_err(|err| {
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
        .map(agent_reel_story::CompiledStory::to_bulletin)
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
    for bulletin in bulletins {
        state
            .reel
            .lock()
            .map_err(|_| {
                tracing::error!("reel buffer lock poisoned during ingest");
                AppError::StatePoisoned
            })?
            .push(bulletin.clone());
        state.metrics.record_emitted();
        match state.tx.send(bulletin) {
            Ok(receivers) => {
                tracing::debug!(%event_id, event_kind, receivers, "story bulletin sent to sse subscribers");
            }
            Err(err) => {
                tracing::debug!(%event_id, event_kind, error = %err, "story bulletin stored without sse subscribers");
            }
        }
    }

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
                    "agent_reel state lock poisoned",
                )
                    .into_response()
            }
        }
    }
}
