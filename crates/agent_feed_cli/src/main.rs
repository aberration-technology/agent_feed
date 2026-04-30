use agent_feed_adapters::claude::{ClaudeState, normalize_stream_value};
use agent_feed_adapters::codex::{TranscriptState, normalize_transcript_value};
use agent_feed_auth_github::{
    GithubAuthError, GithubAuthSession, GithubCliAuthConfig, GithubSessionStore, begin_cli_login,
    complete_cli_login, parse_cli_callback_request,
};
use agent_feed_core::{AgentEvent, EventKind, Severity, SourceKind};
use agent_feed_directory::{RemoteUserRoute, validate_logical_feed_label};
use agent_feed_edge::{
    EdgeConfig, EdgeFabricConfig, EdgeServerConfig, OrgDeploymentPolicy, serve_http,
};
use agent_feed_ingest::source_from_str;
use agent_feed_install::{doctor_report, init_plan};
use agent_feed_p2p::{
    EdgeFallbackMode, MAINNET_EDGE_BASE_URL, P2pDataPlane, P2pDataPlaneCapability, P2pNetworkConfig,
};
use agent_feed_p2p_proto::{
    FeedVisibility, ProtocolCompatibility, PublisherIdentity, Signed, StoryCapsule,
};
use agent_feed_security::SecurityConfig;
use agent_feed_server::{ServerConfig, serve_with_ready};
use agent_feed_story::{
    CompiledStory, StoryCompiler, StoryCompilerDiagnostics, StoryDecision, StoryDecisionAction,
    compile_events,
};
use agent_feed_summarize::{
    DEFAULT_SUMMARY_PROMPT_MAX_CHARS, DEFAULT_SUMMARY_PROMPT_STYLE, FeedSummary, FeedSummaryMode,
    GuardrailPattern, INTERNAL_SUMMARIZER_MARKER, ImageDecisionMode, ImageProcessorConfig,
    RecentSummary, SummaryConfig, SummaryError, SummaryProcessorConfig, summarize_feed,
    summarize_feed_with_recent,
};
use agent_feed_views::{PublishStatusUpdate, PublishStatusView, StatusView};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::fmt::Write as FmtWrite;
use std::fs::{self, File};
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

const DEFAULT_URL: &str = "http://127.0.0.1:7777/reel";
const LOOPBACK_ADDR: &str = "127.0.0.1:7777";
const CAPTURE_STATUS_HEARTBEAT: Duration = Duration::from_secs(15);
const STARTUP_STATE_SAMPLE_BYTES: usize = 1024 * 1024;
const STARTUP_CONTEXT_TAIL_BYTES: usize = 4 * 1024 * 1024;
const STARTUP_RECAP_MAX_AGE: time::Duration = time::Duration::minutes(45);
const STARTUP_CONTEXT_TAG: &str = "startup-context";

#[derive(Debug, Parser)]
#[command(name = "agent-feed", version)]
#[command(about = "agent activity, reduced to signal")]
struct Cli {
    #[arg(
        long,
        global = true,
        visible_alias = "log-filter",
        default_value = "agent_feed=info,agent_feed_cli=info,agent_feed_summarize=info,tower_http=warn",
        help = "tracing filter or level, for example debug or agent_feed=trace"
    )]
    log_level: String,
    #[arg(long, global = true, value_enum, default_value = "compact")]
    log_format: LogFormat,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum LogFormat {
    Compact,
    Pretty,
    Json,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum CliEdgeFallback {
    #[default]
    Auto,
    On,
    Off,
}

impl From<CliEdgeFallback> for EdgeFallbackMode {
    fn from(value: CliEdgeFallback) -> Self {
        match value {
            CliEdgeFallback::Auto => Self::Auto,
            CliEdgeFallback::On => Self::On,
            CliEdgeFallback::Off => Self::Off,
        }
    }
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Commands {
    Doctor {
        #[command(subcommand)]
        command: Option<DoctorCommand>,
    },
    Init {
        #[arg(long)]
        auto: bool,
        #[arg(long)]
        codex: bool,
        #[arg(long)]
        claude: bool,
    },
    Serve {
        #[arg(long, default_value = "127.0.0.1:7777")]
        bind: SocketAddr,
        #[arg(long)]
        display_token_file: Option<PathBuf>,
        #[arg(long)]
        p2p: bool,
        #[arg(long)]
        no_p2p: bool,
        #[arg(
            long,
            help = "publish future summarized stories while serving; implies --p2p and requires github auth"
        )]
        publish: bool,
        #[arg(
            long,
            visible_aliases = ["feed-name", "feed-label"],
            default_value = "workstation",
            value_parser = parse_feed_arg,
            help = "logical feed name for all selected local agent sessions"
        )]
        feed: String,
        #[arg(long, default_value = "https://api.feed.aberration.technology")]
        edge: String,
        #[arg(long, default_value = "agent-feed-mainnet")]
        network_id: String,
        #[arg(long)]
        auth_store: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:0")]
        auth_callback_bind: SocketAddr,
        #[arg(long, default_value_t = 120)]
        auth_timeout_secs: u64,
        #[arg(long)]
        no_auth_browser: bool,
        #[arg(long, default_value_t = 20)]
        publish_interval_secs: u64,
        #[arg(
            long,
            value_enum,
            default_value = "auto",
            help = "edge snapshot fallback policy while native p2p transport is being brought up"
        )]
        edge_fallback: CliEdgeFallback,
        #[arg(long, default_value = "codex,claude")]
        agents: String,
        #[arg(long, default_value_t = 12)]
        sessions: usize,
        #[arg(
            long,
            value_name = "PATH",
            default_value = ".",
            help = "only collect events whose cwd is this workspace or one of its children"
        )]
        workspace: Option<PathBuf>,
        #[arg(long, help = "capture active agent sessions from every workspace")]
        all_workspaces: bool,
        #[arg(long, visible_aliases = ["no-agent-watch", "no-capture"])]
        no_agent_capture: bool,
        #[arg(
            long,
            help = "replay currently selected transcript history before tailing"
        )]
        include_history: bool,
        #[arg(long, default_value_t = 1000)]
        poll_ms: u64,
        #[arg(long)]
        codex_history: Option<PathBuf>,
        #[arg(long)]
        codex_sessions_dir: Option<PathBuf>,
        #[arg(long)]
        claude_projects_dir: Option<PathBuf>,
        #[arg(long, default_value = "codex-memory")]
        summarizer: String,
        #[arg(
            long,
            visible_alias = "headline-style",
            default_value = DEFAULT_SUMMARY_PROMPT_STYLE
        )]
        summary_style: String,
        #[arg(long, default_value_t = DEFAULT_SUMMARY_PROMPT_MAX_CHARS)]
        summary_prompt_max_chars: usize,
        #[arg(long)]
        per_story: bool,
        #[arg(long)]
        allow_project_names: bool,
        #[arg(long)]
        summary_memory_store: Option<PathBuf>,
        #[arg(long)]
        summary_memory_reset: bool,
        #[arg(long)]
        summary_endpoint: Option<String>,
        #[arg(long)]
        summary_auth_header_env: Option<String>,
        #[arg(long)]
        summary_command: Option<String>,
        #[arg(long = "summary-arg")]
        summary_args: Vec<String>,
        #[arg(long)]
        guardrail_pattern: Vec<String>,
    },
    Open {
        #[arg(default_value = DEFAULT_URL)]
        url: String,
    },
    Status {
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
        #[arg(long)]
        json: bool,
    },
    Ingest {
        #[arg(long, default_value = "generic")]
        source: String,
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
    },
    Hook {
        #[arg(long)]
        source: String,
        #[arg(long)]
        event: String,
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Codex {
        #[command(subcommand)]
        command: CodexCommand,
    },
    Claude {
        #[command(subcommand)]
        command: ClaudeCommand,
    },
    P2p {
        #[command(subcommand)]
        command: P2pCommand,
    },
    Edge {
        #[command(subcommand)]
        command: EdgeCommand,
    },
    Uninstall {
        #[arg(long)]
        restore_hooks: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DoctorCommand {
    Publish {
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
        #[arg(long, default_value = "https://api.feed.aberration.technology")]
        edge: String,
        #[arg(long, default_value = "agent-feed-mainnet")]
        network_id: String,
        #[arg(long)]
        auth_store: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Github {
        #[arg(long, default_value = "https://api.feed.aberration.technology")]
        edge: String,
        #[arg(long, default_value = "127.0.0.1:0")]
        callback_bind: SocketAddr,
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
        #[arg(long)]
        no_browser: bool,
        #[arg(long)]
        print_url: bool,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    Status {
        #[arg(long)]
        store: Option<PathBuf>,
    },
    Logout {
        #[arg(long)]
        store: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum CodexCommand {
    Active {
        #[arg(long, default_value_t = 2)]
        sessions: usize,
        #[arg(long)]
        history: Option<PathBuf>,
        #[arg(long)]
        sessions_dir: Option<PathBuf>,
        #[arg(
            long,
            value_name = "PATH",
            help = "only collect events whose cwd is this workspace or one of its children"
        )]
        workspace: Option<PathBuf>,
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 1000)]
        poll_ms: u64,
    },
    Import {
        paths: Vec<PathBuf>,
        #[arg(
            long,
            value_name = "PATH",
            help = "only collect events whose cwd is this workspace or one of its children"
        )]
        workspace: Option<PathBuf>,
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
    },
    Stories {
        #[arg(long, default_value_t = 2)]
        sessions: usize,
        #[arg(long)]
        history: Option<PathBuf>,
        #[arg(long)]
        sessions_dir: Option<PathBuf>,
        #[arg(
            long,
            value_name = "PATH",
            help = "only collect events whose cwd is this workspace or one of its children"
        )]
        workspace: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum ClaudeCommand {
    Active {
        #[arg(long, default_value_t = 2)]
        sessions: usize,
        #[arg(long)]
        projects_dir: Option<PathBuf>,
        #[arg(
            long,
            value_name = "PATH",
            help = "only collect events whose cwd is this workspace or one of its children"
        )]
        workspace: Option<PathBuf>,
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 1000)]
        poll_ms: u64,
    },
    Import {
        paths: Vec<PathBuf>,
        #[arg(
            long,
            value_name = "PATH",
            help = "only collect events whose cwd is this workspace or one of its children"
        )]
        workspace: Option<PathBuf>,
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
    },
    Stream {
        #[arg(
            long,
            value_name = "PATH",
            help = "only collect events whose cwd is this workspace or one of its children"
        )]
        workspace: Option<PathBuf>,
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
    },
    Stories {
        #[arg(long, default_value_t = 2)]
        sessions: usize,
        #[arg(long)]
        projects_dir: Option<PathBuf>,
        #[arg(
            long,
            value_name = "PATH",
            help = "only collect events whose cwd is this workspace or one of its children"
        )]
        workspace: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
enum P2pCommand {
    Init,
    Join {
        network: String,
        #[arg(long)]
        network_id: Option<String>,
        #[arg(long)]
        bootstrap: Vec<String>,
    },
    Status,
    Peers,
    Doctor,
    Discover {
        provider: String,
        target: String,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        streams: Option<String>,
        #[arg(long)]
        team: Option<String>,
        #[arg(long)]
        explain: bool,
    },
    Share {
        #[arg(
            long,
            visible_aliases = ["feed-name", "feed-label"],
            default_value = "workstation",
            value_parser = parse_feed_arg,
        )]
        feed: String,
        #[arg(long, default_value = "private")]
        visibility: String,
        #[arg(long)]
        github_org: Option<String>,
        #[arg(long)]
        github_team: Option<String>,
    },
    Pause,
    Resume,
    Publish {
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value = "https://api.feed.aberration.technology")]
        edge: String,
        #[arg(long, default_value = "agent-feed-mainnet")]
        network_id: String,
        #[arg(
            long,
            value_enum,
            default_value = "auto",
            help = "edge snapshot fallback policy while native p2p transport is being brought up"
        )]
        edge_fallback: CliEdgeFallback,
        #[arg(long)]
        auth_store: Option<PathBuf>,
        #[arg(
            long,
            visible_aliases = ["feed-name", "feed-label"],
            default_value = "workstation",
            value_parser = parse_feed_arg,
        )]
        feed: String,
        #[arg(long, default_value_t = 2)]
        sessions: usize,
        #[arg(long, default_value = "codex,claude")]
        agents: String,
        #[arg(long)]
        history: Option<PathBuf>,
        #[arg(long)]
        sessions_dir: Option<PathBuf>,
        #[arg(long)]
        claude_projects_dir: Option<PathBuf>,
        #[arg(
            long,
            value_name = "PATH",
            help = "only collect events whose cwd is this workspace or one of its children"
        )]
        workspace: Option<PathBuf>,
        #[arg(
            long,
            help = "replay selected transcript history instead of only future deltas"
        )]
        include_history: bool,
        #[arg(long, default_value = "codex-memory")]
        summarizer: String,
        #[arg(
            long,
            visible_alias = "headline-style",
            default_value = DEFAULT_SUMMARY_PROMPT_STYLE
        )]
        summary_style: String,
        #[arg(long, default_value_t = DEFAULT_SUMMARY_PROMPT_MAX_CHARS)]
        summary_prompt_max_chars: usize,
        #[arg(long)]
        per_story: bool,
        #[arg(long)]
        allow_project_names: bool,
        #[arg(long)]
        summary_memory_store: Option<PathBuf>,
        #[arg(long)]
        summary_memory_reset: bool,
        #[arg(long)]
        summary_endpoint: Option<String>,
        #[arg(long)]
        summary_auth_header_env: Option<String>,
        #[arg(long)]
        summary_command: Option<String>,
        #[arg(long = "summary-arg")]
        summary_args: Vec<String>,
        #[arg(long)]
        guardrail_pattern: Vec<String>,
        #[arg(long)]
        images: bool,
        #[arg(long, default_value = "codex-exec")]
        image_processor: String,
        #[arg(long)]
        image_endpoint: Option<String>,
        #[arg(long)]
        image_command: Option<String>,
        #[arg(long = "image-arg")]
        image_args: Vec<String>,
        #[arg(long)]
        image_style: Option<String>,
        #[arg(long)]
        image_prompt_max_chars: Option<usize>,
        #[arg(long)]
        allow_remote_image_urls: bool,
    },
}

#[derive(Debug, Subcommand)]
enum EdgeCommand {
    Serve {
        #[arg(long, default_value = "127.0.0.1:7778")]
        bind: SocketAddr,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, default_value = "https://api.feed.aberration.technology")]
        edge_base_url: String,
        #[arg(long, default_value = "edge.feed.aberration.technology")]
        bootstrap_host: String,
        #[arg(long, default_value = "https://feed.aberration.technology")]
        browser_app_base_url: String,
        #[arg(
            long,
            default_value = "https://feed.aberration.technology/callback/github"
        )]
        github_callback_url: String,
        #[arg(long, default_value = "agent-feed-mainnet")]
        network_id: String,
    },
    Health,
}

#[derive(Debug, thiserror::Error)]
enum CliError {
    #[error(transparent)]
    Server(#[from] agent_feed_server::ServerError),
    #[error(transparent)]
    Adapter(#[from] agent_feed_adapters::AdapterError),
    #[error(transparent)]
    Directory(#[from] agent_feed_directory::DirectoryError),
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("json failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Auth(#[from] GithubAuthError),
    #[error(transparent)]
    Proto(#[from] agent_feed_p2p_proto::ProtoError),
    #[error(transparent)]
    Summary(#[from] SummaryError),
    #[error(transparent)]
    Edge(#[from] agent_feed_edge::EdgeServeError),
    #[error("http request failed: {0}")]
    Http(String),
}

#[tokio::main]
async fn main() -> Result<(), CliError> {
    let cli = Cli::parse();
    init_tracing(&cli.log_level, cli.log_format);

    let command = command_name(&cli.command);
    info!(
        %command,
        log_filter = %cli.log_level,
        log_format = ?cli.log_format,
        "cli command started"
    );
    let result = run_command(cli.command).await;
    if let Err(err) = &result {
        error!(%command, error = %err, "cli command failed");
    } else {
        debug!(%command, "cli command completed");
    }
    result
}

async fn run_command(command: Commands) -> Result<(), CliError> {
    match command {
        Commands::Doctor { command } => match command {
            Some(DoctorCommand::Publish {
                server,
                edge,
                network_id,
                auth_store,
                json,
            }) => {
                info!(%server, %edge, %network_id, json, "publish doctor requested");
                let report = doctor_publish(&server, &edge, &network_id, auth_store);
                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    print!("{}", format_publish_doctor_report(&report));
                }
            }
            None => {
                let report = doctor_report();
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
        },
        Commands::Init {
            auto,
            codex,
            claude,
        } => {
            for step in init_plan(auto, codex, claude) {
                println!("{step}");
            }
        }
        Commands::Serve {
            bind,
            display_token_file,
            p2p,
            no_p2p,
            publish,
            feed,
            edge,
            network_id,
            auth_store,
            auth_callback_bind,
            auth_timeout_secs,
            no_auth_browser,
            publish_interval_secs,
            edge_fallback,
            agents,
            sessions,
            workspace,
            all_workspaces,
            no_agent_capture,
            include_history,
            poll_ms,
            codex_history,
            codex_sessions_dir,
            claude_projects_dir,
            summarizer,
            summary_style,
            summary_prompt_max_chars,
            per_story: _,
            allow_project_names,
            summary_memory_store,
            summary_memory_reset,
            summary_endpoint,
            summary_auth_header_env,
            summary_command,
            summary_args,
            guardrail_pattern,
        } => {
            let display_token = display_token_file
                .map(fs::read_to_string)
                .transpose()?
                .map(|token| token.trim().to_string())
                .filter(|token| !token.is_empty());
            let security = SecurityConfig {
                bind,
                display_token,
                ..SecurityConfig::default()
            };
            if publish && no_p2p {
                return Err(CliError::Http(
                    "`agent-feed serve --publish` cannot be combined with --no-p2p".to_string(),
                ));
            }
            if publish && no_agent_capture {
                return Err(CliError::Http(
                    "`agent-feed serve --publish` needs agent capture; remove --no-agent-capture"
                        .to_string(),
                ));
            }
            let edge_fallback = EdgeFallbackMode::from(edge_fallback);
            if publish && edge_fallback == EdgeFallbackMode::Off {
                return Err(CliError::Http(
                    "`agent-feed serve --publish --edge-fallback off` needs the native p2p data plane, which is not enabled in this build yet; use --edge-fallback auto for the single-bootstrap edge snapshot path".to_string(),
                ));
            }
            let p2p_enabled = (p2p || publish) && !no_p2p;
            let p2p_network = P2pNetworkConfig {
                network_id: network_id.clone(),
                edge_base_url: edge.clone(),
                ..P2pNetworkConfig::mainnet_single_bootstrap(edge_fallback)
            };
            let publish_sink = if publish {
                let selected_agents = parse_agent_list(&agents);
                let mut summary_config = summary_config(SummaryCliOptions {
                    summarizer: &summarizer,
                    summary_style: &summary_style,
                    summary_prompt_max_chars,
                    per_story: true,
                    allow_project_names,
                    summary_memory_store: summary_memory_store.as_deref(),
                    summary_endpoint: summary_endpoint.as_deref(),
                    summary_auth_header_env: summary_auth_header_env.as_deref(),
                    summary_command: summary_command.as_deref(),
                    summary_args: &summary_args,
                    guardrail_patterns: &guardrail_pattern,
                    images: false,
                    image_processor: "disabled",
                    image_endpoint: None,
                    image_command: None,
                    image_args: &[],
                    image_style: None,
                    image_prompt_max_chars: None,
                    allow_remote_image_urls: false,
                })?;
                let workspace_filter_for_memory = if all_workspaces {
                    None
                } else {
                    WorkspaceFilter::from_cli(workspace.clone())?
                };
                scope_summary_memory(
                    &mut summary_config,
                    &feed,
                    &selected_agents,
                    workspace_filter_for_memory.as_ref(),
                    summary_memory_reset,
                )?;
                let processor_registry =
                    ProcessorSessionRegistry::from_summary_config(&summary_config);
                let session = ensure_publish_session(
                    auth_store,
                    &edge,
                    auth_callback_bind,
                    Duration::from_secs(auth_timeout_secs),
                    no_auth_browser,
                )?;
                let publisher = PublisherIdentity::github(
                    session.github_user_id,
                    session.login.clone(),
                    session.name.clone(),
                    session.avatar_url.clone(),
                );
                let (sender, receiver) = mpsc::channel();
                spawn_serve_publisher(ServePublishConfig {
                    receiver,
                    server: bind.to_string(),
                    edge: edge.clone(),
                    network_id: network_id.clone(),
                    feed: feed.clone(),
                    session,
                    publisher,
                    summary_config,
                    publish_interval: Duration::from_secs(publish_interval_secs.max(1)),
                    edge_fallback,
                    processor_registry: processor_registry.clone(),
                });
                Some((sender, processor_registry))
            } else {
                None
            };
            info!(
                %bind,
                p2p_enabled,
                p2p_requested = p2p,
                no_p2p,
                publish,
                feed_name = %feed,
                %edge,
                %network_id,
                edge_fallback = edge_fallback.as_str(),
                data_plane = p2p_network.data_plane.as_str(),
                topology = p2p_network.topology.as_str(),
                display_token_configured = security.display_token.is_some(),
                "serving local feed"
            );
            let capture = if no_agent_capture {
                info!("agent transcript capture disabled for serve");
                None
            } else {
                let workspace_filter = if all_workspaces {
                    info!("serve agent capture scanning all workspaces");
                    None
                } else {
                    WorkspaceFilter::from_cli(workspace)?
                };
                Some(ServeAgentCapture {
                    agents,
                    sessions,
                    workspace: workspace_filter,
                    include_history,
                    poll_ms,
                    codex_history,
                    codex_sessions_dir,
                    claude_projects_dir,
                    event_sink: publish_sink.as_ref().map(|(sender, _)| sender.clone()),
                    processor_registry: publish_sink
                        .as_ref()
                        .map(|(_, registry)| registry.clone())
                        .unwrap_or_default(),
                })
            };
            serve_with_ready(
                ServerConfig {
                    security,
                    p2p_enabled,
                },
                move |bind| {
                    println!("serving http://{bind}/reel");
                    if p2p_enabled {
                        if publish {
                            println!(
                                "p2p publish enabled: future summarized stories publish as feed `{feed}` via edge snapshot fallback"
                            );
                        } else {
                            println!(
                                "p2p discovery ux enabled; add --publish --feed <name> to publish future summarized stories from this serve process"
                            );
                        }
                    }
                    if let Some(capture) = capture {
                        start_serve_agent_capture(bind, capture);
                    } else {
                        println!("agent capture disabled; use `agent-feed codex active --watch` to attach manually");
                    }
                },
            )
            .await?;
        }
        Commands::Open { url } => {
            info!(%url, "opening feed url");
            open_url(&url)?;
            println!("{url}");
        }
        Commands::Status { server, json } => {
            info!(%server, json, "local feed status requested");
            let status = get_json::<StatusView>(&server, "/api/status")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print!("{}", format_local_status(&status));
            }
        }
        Commands::Ingest { source, server } => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let path = endpoint_for_source(&source);
            let payloads = payloads_from_input(&input)?;
            info!(
                %source,
                %server,
                endpoint = path,
                payloads = payloads.len(),
                bytes = input.len(),
                "ingest forwarding payload batch"
            );
            for (index, payload) in payloads.iter().enumerate() {
                debug!(%source, endpoint = path, index, bytes = payload.len(), "ingest payload outgoing");
                println!("{}", post_json(&server, path, payload)?);
            }
        }
        Commands::Hook {
            source,
            event,
            server,
        } => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let payload = hook_payload(&source, &event, &input);
            let endpoint = endpoint_for_source(&source);
            info!(
                %source,
                %event,
                %server,
                endpoint,
                bytes = input.len(),
                "hook forwarding event"
            );
            if let Err(err) = post_json(&server, endpoint, &payload) {
                warn!(%source, %event, %server, endpoint, error = %err, "hook telemetry failed open");
            }
        }
        Commands::Auth { command } => match command {
            AuthCommand::Github {
                edge,
                callback_bind,
                timeout_secs,
                no_browser,
                print_url,
                store,
            } => {
                let session = github_cli_login(
                    edge,
                    callback_bind,
                    Duration::from_secs(timeout_secs),
                    no_browser,
                    print_url,
                    store,
                )?;
                println!(
                    "github auth: @{} github:{} expires {}",
                    session.login, session.github_user_id, session.expires_at
                );
            }
            AuthCommand::Status { store } => {
                let store = GithubSessionStore::new(auth_store_path(store));
                match store.load()? {
                    Some(session) => {
                        info!(
                            github_login = %session.login,
                            github_user_id = session.github_user_id,
                            expired = session.is_expired_at(time::OffsetDateTime::now_utc()),
                            "github auth status loaded"
                        );
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "provider": "github",
                                "login": session.login,
                                "github_user_id": session.github_user_id,
                                "name": session.name,
                                "avatar_url": session.avatar_url,
                                "edge_base_url": session.edge_base_url,
                                "expires_at": session.expires_at,
                                "expired": session.is_expired_at(time::OffsetDateTime::now_utc()),
                            }))?
                        );
                    }
                    None => {
                        info!("github auth status signed out");
                        println!("github auth: signed out");
                    }
                }
            }
            AuthCommand::Logout { store } => {
                let store = GithubSessionStore::new(auth_store_path(store));
                if store.delete()? {
                    info!("github auth session deleted");
                    println!("github auth: signed out");
                } else {
                    info!("github auth logout requested while signed out");
                    println!("github auth: already signed out");
                }
            }
        },
        Commands::Codex { command } => match command {
            CodexCommand::Active {
                sessions,
                history,
                sessions_dir,
                workspace,
                server,
                watch,
                poll_ms,
            } => {
                let history = history.unwrap_or_else(default_codex_history);
                let sessions_dir = sessions_dir.unwrap_or_else(default_codex_sessions_dir);
                let workspace_filter = WorkspaceFilter::from_cli(workspace)?;
                let paths = active_codex_session_paths(
                    &history,
                    &sessions_dir,
                    sessions,
                    workspace_filter.as_ref(),
                    None,
                )?;
                info!(
                    sessions_requested = sessions,
                    sessions_found = paths.len(),
                    workspace = workspace_filter.log_value(),
                    %server,
                    watch,
                    poll_ms,
                    history = %history.display(),
                    sessions_dir = %sessions_dir.display(),
                    "codex active session capture starting"
                );
                if watch {
                    watch_codex_sessions(
                        &server,
                        &paths,
                        poll_ms,
                        workspace_filter.as_ref(),
                        true,
                        None,
                        None,
                    )?;
                } else {
                    import_codex_sessions(&server, &paths, workspace_filter.as_ref())?;
                }
            }
            CodexCommand::Import {
                paths,
                workspace,
                server,
            } => {
                let workspace_filter = WorkspaceFilter::from_cli(workspace)?;
                info!(
                    sessions = paths.len(),
                    workspace = workspace_filter.log_value(),
                    %server,
                    "codex transcript import starting"
                );
                import_codex_sessions(&server, &paths, workspace_filter.as_ref())?;
            }
            CodexCommand::Stories {
                sessions,
                history,
                sessions_dir,
                workspace,
            } => {
                let history = history.unwrap_or_else(default_codex_history);
                let sessions_dir = sessions_dir.unwrap_or_else(default_codex_sessions_dir);
                let workspace_filter = WorkspaceFilter::from_cli(workspace)?;
                let paths = active_codex_session_paths(
                    &history,
                    &sessions_dir,
                    sessions,
                    workspace_filter.as_ref(),
                    None,
                )?;
                info!(
                    sessions_requested = sessions,
                    sessions_found = paths.len(),
                    workspace = workspace_filter.log_value(),
                    "codex story compilation starting"
                );
                let stories = compile_codex_stories(&paths, workspace_filter.as_ref())?;
                print_stories("codex", &paths, &stories)?;
            }
        },
        Commands::Claude { command } => match command {
            ClaudeCommand::Active {
                sessions,
                projects_dir,
                workspace,
                server,
                watch,
                poll_ms,
            } => {
                let projects_dir = projects_dir.unwrap_or_else(default_claude_projects_dir);
                let workspace_filter = WorkspaceFilter::from_cli(workspace)?;
                let paths = active_claude_session_paths(
                    &projects_dir,
                    sessions,
                    workspace_filter.as_ref(),
                )?;
                info!(
                    sessions_requested = sessions,
                    sessions_found = paths.len(),
                    workspace = workspace_filter.log_value(),
                    %server,
                    watch,
                    poll_ms,
                    projects_dir = %projects_dir.display(),
                    "claude active session capture starting"
                );
                if watch {
                    watch_claude_sessions(
                        &server,
                        &paths,
                        poll_ms,
                        workspace_filter.as_ref(),
                        true,
                        None,
                    )?;
                } else {
                    import_claude_sessions(&server, &paths, workspace_filter.as_ref())?;
                }
            }
            ClaudeCommand::Import {
                paths,
                workspace,
                server,
            } => {
                let workspace_filter = WorkspaceFilter::from_cli(workspace)?;
                info!(
                    sessions = paths.len(),
                    workspace = workspace_filter.log_value(),
                    %server,
                    "claude stream import starting"
                );
                import_claude_sessions(&server, &paths, workspace_filter.as_ref())?;
            }
            ClaudeCommand::Stream { workspace, server } => {
                let workspace_filter = WorkspaceFilter::from_cli(workspace)?;
                let mut input = String::new();
                std::io::stdin().read_to_string(&mut input)?;
                info!(
                    %server,
                    bytes = input.len(),
                    workspace = workspace_filter.log_value(),
                    "claude stream stdin import starting"
                );
                let stats = import_claude_stream_input(
                    &server,
                    Path::new("<stdin>"),
                    &input,
                    workspace_filter.as_ref(),
                );
                print_import_complete("claude stream", stats);
            }
            ClaudeCommand::Stories {
                sessions,
                projects_dir,
                workspace,
            } => {
                let projects_dir = projects_dir.unwrap_or_else(default_claude_projects_dir);
                let workspace_filter = WorkspaceFilter::from_cli(workspace)?;
                let paths = active_claude_session_paths(
                    &projects_dir,
                    sessions,
                    workspace_filter.as_ref(),
                )?;
                info!(
                    sessions_requested = sessions,
                    sessions_found = paths.len(),
                    workspace = workspace_filter.log_value(),
                    "claude story compilation starting"
                );
                let stories = compile_claude_stories(&paths, workspace_filter.as_ref())?;
                print_stories("claude", &paths, &stories)?;
            }
        },
        Commands::P2p { command } => match command {
            P2pCommand::Init => {
                info!("p2p config init requested");
                println!("p2p config initialized: p2p.enabled=false, publish.enabled=false");
            }
            P2pCommand::Join {
                network,
                network_id,
                bootstrap,
            } => {
                let network_id = network_id.unwrap_or_else(|| {
                    if network == "mainnet" {
                        "agent-feed-mainnet".to_string()
                    } else {
                        "agent-feed-custom".to_string()
                    }
                });
                info!(
                    %network,
                    %network_id,
                    bootstrap_peers = bootstrap.len(),
                    "p2p join staged"
                );
                println!("p2p join staged: network={network} network_id={network_id}");
                for peer in bootstrap {
                    debug!(bootstrap_peer = %peer, "p2p bootstrap peer configured");
                    println!("bootstrap {peer}");
                }
            }
            P2pCommand::Status => {
                let config = P2pNetworkConfig::mainnet_single_bootstrap(EdgeFallbackMode::Auto);
                let status = config.status();
                let transport = status.transport_capability();
                let data_planes = config
                    .transport_capabilities()
                    .iter()
                    .map(p2p_capability_json)
                    .collect::<Vec<_>>();
                info!(
                    network_id = %status.network_id,
                    data_plane = status.data_plane.as_str(),
                    topology = status.topology.as_str(),
                    bootstrap_peers = status.bootstrap_peers.len(),
                    transport_available = transport.available,
                    "p2p status requested"
                );
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "network_id": status.network_id,
                        "compatibility": status.compatibility,
                        "data_plane": status.data_plane.as_str(),
                        "projection_label": status.projection_label(),
                        "topology": status.topology.as_str(),
                        "edge_fallback": status.edge_fallback.as_str(),
                        "bootstrap_peers": status.bootstrap_peers,
                        "transport": p2p_capability_json(&transport),
                        "data_planes": data_planes,
                        "fabric_peers": status.fabric_peers,
                        "subscribed_feeds": status.subscribed_feeds,
                        "publishing": status.publishing,
                        "note": "current production uses one bootstrap edge and edge snapshot fallback; native and browser libp2p transports are not enabled in this build",
                    }))?
                );
            }
            P2pCommand::Peers => {
                info!("p2p peers requested");
                println!("native p2p runtime is not running in this process");
                println!("single bootstrap edge: {MAINNET_EDGE_BASE_URL}");
            }
            P2pCommand::Doctor => {
                let config = P2pNetworkConfig::mainnet_single_bootstrap(EdgeFallbackMode::Auto);
                let active = config.active_transport_capability();
                info!(
                    network_id = %config.network_id,
                    topology = config.topology.as_str(),
                    bootstrap_peers = config.bootstrap_peers.len(),
                    single_bootstrap = config.is_single_bootstrap_topology(),
                    data_plane = active.data_plane.as_str(),
                    transport_available = active.available,
                    "p2p doctor requested"
                );
                println!("p2p doctor: story capsule protocol ok");
                println!("p2p doctor: topology={}", config.topology.as_str());
                println!("p2p doctor: bootstrap_host=edge.feed.aberration.technology");
                println!("p2p doctor: data_plane={}", config.data_plane.as_str());
                println!(
                    "p2p doctor: active_transport={} available={} · {}",
                    active.data_plane.as_str(),
                    active.available,
                    active.reason
                );
                for capability in config.transport_capabilities() {
                    let status = if capability.available {
                        "available"
                    } else {
                        "unavailable"
                    };
                    println!(
                        "p2p doctor: {} {status} · protocols={} · transports={} · {}",
                        capability.data_plane.as_str(),
                        capability.protocols.join(","),
                        capability.transports.join(","),
                        capability.next_step
                    );
                }
            }
            P2pCommand::Discover {
                provider,
                target,
                all,
                streams,
                team,
                explain,
            } => {
                let query = discover_query(all, streams.as_deref());
                info!(
                    %provider,
                    %target,
                    all,
                    streams = streams.as_deref().unwrap_or(""),
                    team = team.as_deref().unwrap_or(""),
                    explain,
                    "p2p discovery query compiled"
                );
                match provider.as_str() {
                    "github" => {
                        let route = RemoteUserRoute::parse(&format!("/{target}"), Some(&query))?;
                        if explain {
                            println!("github lookup alias: @{target}");
                            println!(
                                "durable network identity: github numeric user id from edge resolver"
                            );
                            println!(
                                "privacy: story_only={} raw_events={} require_settled={}",
                                route.reel_filter.story_only,
                                route.reel_filter.raw_events,
                                route.reel_filter.require_settled
                            );
                        }
                        println!("{}", serde_json::to_string_pretty(&route)?);
                    }
                    "github-org" | "org" => {
                        let filter =
                            agent_feed_directory::OrgRouteFilter::from_query(Some(&query))?;
                        if explain {
                            println!("github org discovery: {target}");
                            println!(
                                "delivery: no automatic subscription; explicit follow required"
                            );
                            println!(
                                "privacy: story_only={} raw_events={} require_settled={}",
                                filter.reel_filter.story_only,
                                filter.reel_filter.raw_events,
                                filter.reel_filter.require_settled
                            );
                        }
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "provider": "github-org",
                                "org": target,
                                "team": team,
                                "filter": filter,
                            }))?
                        );
                    }
                    "github-team" | "team" => {
                        let Some(team) = team else {
                            return Err(CliError::Http(
                                "p2p discover github-team requires --team".to_string(),
                            ));
                        };
                        let filter =
                            agent_feed_directory::OrgRouteFilter::from_query(Some(&query))?;
                        if explain {
                            println!("github team discovery: {target}/{team}");
                            println!(
                                "delivery: no automatic subscription; explicit follow required"
                            );
                        }
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "provider": "github-team",
                                "org": target,
                                "team": team,
                                "filter": filter,
                            }))?
                        );
                    }
                    _ => {
                        return Err(CliError::Http(
                            "p2p discover supports provider=github, github-org, github-team"
                                .to_string(),
                        ));
                    }
                }
            }
            P2pCommand::Share {
                feed,
                visibility,
                github_org,
                github_team,
            } => {
                let visibility = parse_visibility(&visibility);
                info!(
                    feed_name = %feed,
                    visibility = ?visibility,
                    github_org = github_org.as_deref().unwrap_or(""),
                    github_team = github_team.as_deref().unwrap_or(""),
                    "p2p share staged"
                );
                println!("p2p share staged: feed_name={feed} visibility={visibility:?}");
                if let Some(org) = github_org {
                    println!("github_org={org}");
                }
                if let Some(team) = github_team {
                    println!("github_team={team}");
                }
                println!("raw_events=false summary_only=true encrypt_private_feeds=true");
            }
            P2pCommand::Pause => {
                info!("p2p publish pause requested");
                println!("p2p publish paused");
            }
            P2pCommand::Resume => {
                info!("p2p publish resume requested");
                println!("p2p publish resumed");
            }
            P2pCommand::Publish {
                dry_run,
                edge,
                network_id,
                edge_fallback,
                auth_store,
                feed,
                sessions,
                agents,
                history,
                sessions_dir,
                claude_projects_dir,
                workspace,
                include_history,
                summarizer,
                summary_style,
                summary_prompt_max_chars,
                per_story,
                allow_project_names,
                summary_memory_store,
                summary_memory_reset,
                summary_endpoint,
                summary_auth_header_env,
                summary_command,
                summary_args,
                guardrail_pattern,
                images,
                image_processor,
                image_endpoint,
                image_command,
                image_args,
                image_style,
                image_prompt_max_chars,
                allow_remote_image_urls,
            } => {
                let selected_agents = parse_agent_list(&agents);
                let workspace_filter = WorkspaceFilter::from_cli(workspace)?;
                info!(
                    feed_name = %feed,
                    %edge,
                    %network_id,
                    edge_fallback = EdgeFallbackMode::from(edge_fallback).as_str(),
                    agents = %agents,
                    selected_agents = ?selected_agents,
                    sessions,
                    workspace = workspace_filter.log_value(),
                    include_history,
                    summarizer = %summarizer,
                    per_story,
                    images,
                    dry_run,
                    "p2p publish capture starting"
                );
                let mut captured_paths = Vec::new();
                let mut stories = Vec::new();
                if selected_agents.contains("codex") {
                    let history = history.unwrap_or_else(default_codex_history);
                    let sessions_dir = sessions_dir.unwrap_or_else(default_codex_sessions_dir);
                    let paths = active_codex_session_paths(
                        &history,
                        &sessions_dir,
                        sessions,
                        workspace_filter.as_ref(),
                        None,
                    )?;
                    if include_history {
                        stories.extend(compile_codex_stories(&paths, workspace_filter.as_ref())?);
                    } else {
                        warm_codex_paths(&paths);
                    }
                    captured_paths.extend(paths);
                }
                if selected_agents.contains("claude") {
                    let projects_dir =
                        claude_projects_dir.unwrap_or_else(default_claude_projects_dir);
                    let paths = active_claude_session_paths(
                        &projects_dir,
                        sessions,
                        workspace_filter.as_ref(),
                    )?;
                    if include_history {
                        stories.extend(compile_claude_stories(&paths, workspace_filter.as_ref())?);
                    } else {
                        warm_claude_paths(&paths);
                    }
                    captured_paths.extend(paths);
                }
                let mut summary_config = summary_config(SummaryCliOptions {
                    summarizer: &summarizer,
                    summary_style: &summary_style,
                    summary_prompt_max_chars,
                    per_story,
                    allow_project_names,
                    summary_memory_store: summary_memory_store.as_deref(),
                    summary_endpoint: summary_endpoint.as_deref(),
                    summary_auth_header_env: summary_auth_header_env.as_deref(),
                    summary_command: summary_command.as_deref(),
                    summary_args: &summary_args,
                    guardrail_patterns: &guardrail_pattern,
                    images,
                    image_processor: &image_processor,
                    image_endpoint: image_endpoint.as_deref(),
                    image_command: image_command.as_deref(),
                    image_args: &image_args,
                    image_style: image_style.as_deref(),
                    image_prompt_max_chars,
                    allow_remote_image_urls,
                })?;
                scope_summary_memory(
                    &mut summary_config,
                    &feed,
                    &selected_agents,
                    workspace_filter.as_ref(),
                    summary_memory_reset,
                )?;
                let session = if dry_run {
                    None
                } else {
                    if EdgeFallbackMode::from(edge_fallback) == EdgeFallbackMode::Off {
                        return Err(CliError::Http(
                            "`agent-feed p2p publish --edge-fallback off` needs the native p2p data plane, which is not enabled in this build yet; use --edge-fallback auto".to_string(),
                        ));
                    }
                    Some(load_publish_session(auth_store, &edge)?)
                };
                let publisher = session.as_ref().map(|session| {
                    PublisherIdentity::github(
                        session.github_user_id,
                        session.login.clone(),
                        session.name.clone(),
                        session.avatar_url.clone(),
                    )
                });
                let capsules = signed_capsules(
                    &feed,
                    &stories,
                    &summary_config,
                    publisher.as_ref(),
                    session
                        .as_ref()
                        .and_then(|session| session.session_token.as_deref()),
                )?;
                info!(
                    feed_name = %feed,
                    capsules = capsules.len(),
                    stories = stories.len(),
                    local_agent_sessions = captured_paths.len(),
                    processor = ?summary_config.processor,
                    image_enabled = summary_config.image.enabled,
                    dry_run,
                    "p2p publish summarized"
                );
                if dry_run {
                    println!(
                        "p2p publish dry-run: feed_name={} capsules={} stories={} local_agent_sessions={}",
                        feed,
                        capsules.len(),
                        stories.len(),
                        captured_paths.len()
                    );
                    println!(
                        "logical feed bundle: all selected agent sessions publish under {feed}"
                    );
                    for capsule in capsules.iter().take(8) {
                        debug!(
                            feed_id = %capsule.value.feed_id,
                            capsule_id = %capsule.value.capsule_id,
                            seq = capsule.value.seq,
                            score = capsule.value.score,
                            story_kind = ?capsule.value.story_kind,
                            "p2p capsule outgoing dry-run"
                        );
                        println!("{}", serde_json::to_string(capsule)?);
                    }
                } else if let Some(session) = session {
                    let body = serde_json::to_string(&json!({
                        "network_id": network_id,
                        "compatibility": ProtocolCompatibility::current(),
                        "feed_name": feed,
                        "capsules": capsules,
                    }))?;
                    let response =
                        post_edge_json_with_bearer(&edge, "/network/publish", &body, &session)?;
                    info!(
                        feed_name = %feed,
                        response = %response.trim(),
                        "p2p publish sent to edge"
                    );
                    println!(
                        "p2p publish sent: feed_name={} capsules={} stories={} local_agent_sessions={}",
                        feed,
                        capsules.len(),
                        stories.len(),
                        captured_paths.len()
                    );
                }
            }
        },
        Commands::Edge { command } => match command {
            EdgeCommand::Serve {
                bind,
                config: _,
                edge_base_url,
                bootstrap_host,
                browser_app_base_url,
                github_callback_url,
                network_id,
            } => {
                let bootstrap_host = bootstrap_host
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_end_matches('/')
                    .to_string();
                info!(
                    %bind,
                    edge_base_url = %edge_base_url,
                    bootstrap_host = %bootstrap_host,
                    browser_app_base_url = %browser_app_base_url,
                    github_callback_url = %github_callback_url,
                    network_id = %network_id,
                    "edge server starting"
                );
                let edge = EdgeConfig {
                    network_id,
                    edge_domain: edge_base_url.clone(),
                    browser_app_base_url,
                    github_callback_url,
                    bootstrap_peers: vec![
                        format!("/dns4/{bootstrap_host}/tcp/7747"),
                        format!("/dns4/{bootstrap_host}/udp/7747/quic-v1"),
                        format!("/dns4/{bootstrap_host}/udp/443/webrtc-direct"),
                    ],
                    authority_id: "edge.feed".to_string(),
                    org_policy: OrgDeploymentPolicy::from_env(),
                };
                serve_http(EdgeServerConfig {
                    bind,
                    edge,
                    fabric: EdgeFabricConfig::from_env(),
                })
                .await?;
            }
            EdgeCommand::Health => {
                info!("edge health command requested");
                println!("feed edge: healthz=/healthz readyz=/readyz");
            }
        },
        Commands::Uninstall { restore_hooks } => {
            info!(restore_hooks, "uninstall requested");
            if restore_hooks {
                println!("restore-hooks requested; no installed hook manifest exists yet");
            } else {
                println!("no installed hook manifest exists yet");
            }
        }
    }
    Ok(())
}

fn init_tracing(filter: &str, format: LogFormat) {
    let ansi = std::io::stderr().is_terminal();
    match format {
        LogFormat::Compact => tracing_subscriber::fmt()
            .compact()
            .with_writer(std::io::stderr)
            .with_ansi(ansi)
            .with_target(true)
            .with_env_filter(log_filter(filter))
            .init(),
        LogFormat::Pretty => tracing_subscriber::fmt()
            .pretty()
            .with_writer(std::io::stderr)
            .with_ansi(ansi)
            .with_target(true)
            .with_env_filter(log_filter(filter))
            .init(),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_writer(std::io::stderr)
            .flatten_event(true)
            .with_ansi(false)
            .with_current_span(false)
            .with_env_filter(log_filter(filter))
            .init(),
    }
}

fn log_filter(filter: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_new(filter)
        .or_else(|_| {
            tracing_subscriber::EnvFilter::try_new(format!("agent_feed={filter},tower_http=warn"))
        })
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(
                "agent_feed=info,agent_feed_cli=info,agent_feed_summarize=info,tower_http=warn",
            )
        })
}

fn command_name(command: &Commands) -> &'static str {
    match command {
        Commands::Doctor { command } => match command {
            Some(DoctorCommand::Publish { .. }) => "doctor.publish",
            None => "doctor",
        },
        Commands::Init { .. } => "init",
        Commands::Serve { .. } => "serve",
        Commands::Open { .. } => "open",
        Commands::Status { .. } => "status",
        Commands::Ingest { .. } => "ingest",
        Commands::Hook { .. } => "hook",
        Commands::Auth { command } => match command {
            AuthCommand::Github { .. } => "auth.github",
            AuthCommand::Status { .. } => "auth.status",
            AuthCommand::Logout { .. } => "auth.logout",
        },
        Commands::Codex { command } => match command {
            CodexCommand::Active { .. } => "codex.active",
            CodexCommand::Import { .. } => "codex.import",
            CodexCommand::Stories { .. } => "codex.stories",
        },
        Commands::Claude { command } => match command {
            ClaudeCommand::Active { .. } => "claude.active",
            ClaudeCommand::Import { .. } => "claude.import",
            ClaudeCommand::Stream { .. } => "claude.stream",
            ClaudeCommand::Stories { .. } => "claude.stories",
        },
        Commands::P2p { command } => match command {
            P2pCommand::Init => "p2p.init",
            P2pCommand::Join { .. } => "p2p.join",
            P2pCommand::Status => "p2p.status",
            P2pCommand::Peers => "p2p.peers",
            P2pCommand::Doctor => "p2p.doctor",
            P2pCommand::Discover { .. } => "p2p.discover",
            P2pCommand::Share { .. } => "p2p.share",
            P2pCommand::Pause => "p2p.pause",
            P2pCommand::Resume => "p2p.resume",
            P2pCommand::Publish { .. } => "p2p.publish",
        },
        Commands::Edge { command } => match command {
            EdgeCommand::Serve { .. } => "edge.serve",
            EdgeCommand::Health => "edge.health",
        },
        Commands::Uninstall { .. } => "uninstall",
    }
}

fn discover_query(all: bool, streams: Option<&str>) -> String {
    if all {
        "all".to_string()
    } else if let Some(streams) = streams {
        format!("streams={streams}")
    } else {
        String::new()
    }
}

fn parse_feed_arg(value: &str) -> Result<String, String> {
    validate_logical_feed_label(value)
        .map(|()| value.to_string())
        .map_err(|err| err.to_string())
}

#[derive(Clone, Debug)]
struct WorkspaceFilter {
    root: PathBuf,
}

impl WorkspaceFilter {
    fn from_cli(path: Option<PathBuf>) -> Result<Option<Self>, CliError> {
        path.map(|path| {
            let root = normalize_absolute_path(expand_home_path(path))?;
            info!(workspace = %root.display(), "workspace capture filter enabled");
            Ok(Self { root })
        })
        .transpose()
    }

    fn allows(&self, event: &AgentEvent) -> bool {
        let Some(cwd) = event.cwd.as_deref() else {
            return false;
        };
        let Ok(cwd) = normalize_absolute_path(expand_home_path(PathBuf::from(cwd))) else {
            return false;
        };
        cwd == self.root || cwd.starts_with(&self.root)
    }

    fn display(&self) -> String {
        self.root.display().to_string()
    }
}

trait WorkspaceFilterLogValue {
    fn log_value(&self) -> &str;
}

impl WorkspaceFilterLogValue for Option<WorkspaceFilter> {
    fn log_value(&self) -> &str {
        self.as_ref()
            .map(|filter| filter.root.to_str().unwrap_or("<non-utf8>"))
            .unwrap_or("all")
    }
}

#[derive(Clone, Debug, Default)]
struct ProcessorSessionRegistry {
    inner: Arc<Mutex<ProcessorSessionRegistryState>>,
}

#[derive(Debug, Default)]
struct ProcessorSessionRegistryState {
    summary_memory_paths: Vec<PathBuf>,
    processor_work_dirs: Vec<PathBuf>,
    session_ids: HashSet<String>,
    skipped_session_ids: HashSet<String>,
    processor_events_dropped: u64,
    processor_sessions_skipped: u64,
    ambiguous_internal_candidates: u64,
    last_refresh: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ProcessorSessionRegistrySnapshot {
    known_sessions: usize,
    processor_events_dropped: u64,
    processor_sessions_skipped: u64,
    ambiguous_internal_candidates: u64,
}

impl ProcessorSessionRegistry {
    fn from_summary_config(config: &SummaryConfig) -> Self {
        let registry = Self::default();
        if let Some(path) = summary_memory_path(config) {
            registry.add_processor_work_dir(summary_memory_work_dir(&path));
            registry.add_summary_memory_path(path);
            registry.refresh();
        }
        registry
    }

    fn add_summary_memory_path(&self, path: PathBuf) {
        let Ok(mut state) = self.inner.lock() else {
            warn!("processor session registry lock poisoned while adding summary memory path");
            return;
        };
        if !state.summary_memory_paths.contains(&path) {
            state.summary_memory_paths.push(path);
        }
    }

    fn add_processor_work_dir(&self, path: PathBuf) {
        let Ok(mut state) = self.inner.lock() else {
            warn!("processor session registry lock poisoned while adding processor work dir");
            return;
        };
        let path = clean_path(&expand_home_path(path));
        if !state.processor_work_dirs.contains(&path) {
            state.processor_work_dirs.push(path);
        }
    }

    fn maybe_refresh(&self) {
        let should_refresh = self.inner.lock().is_ok_and(|state| {
            state
                .last_refresh
                .is_none_or(|last_refresh| last_refresh.elapsed() >= Duration::from_secs(5))
        });
        if should_refresh {
            self.refresh();
        }
    }

    fn refresh(&self) {
        let paths = match self.inner.lock() {
            Ok(state) => state.summary_memory_paths.clone(),
            Err(_) => {
                warn!("processor session registry lock poisoned before refresh");
                return;
            }
        };
        let mut discovered = HashSet::new();
        for path in paths {
            for session_id in summary_memory_codex_session_ids(&path) {
                discovered.insert(session_id);
            }
        }
        let Ok(mut state) = self.inner.lock() else {
            warn!("processor session registry lock poisoned during refresh");
            return;
        };
        for session_id in discovered {
            state.session_ids.insert(session_id);
        }
        state.last_refresh = Some(Instant::now());
    }

    #[cfg(test)]
    fn register_session(&self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        if session_id.trim().is_empty() {
            return;
        }
        let Ok(mut state) = self.inner.lock() else {
            warn!("processor session registry lock poisoned while registering session");
            return;
        };
        state.session_ids.insert(session_id);
    }

    fn is_known_session_id(&self, session_id: &str) -> bool {
        if session_id.trim().is_empty() {
            return false;
        }
        self.maybe_refresh();
        self.inner
            .lock()
            .is_ok_and(|state| state.session_ids.contains(session_id))
    }

    fn owns_cwd(&self, cwd: &str) -> bool {
        let cwd = clean_path(&expand_home_path(PathBuf::from(cwd)));
        self.inner.lock().is_ok_and(|state| {
            state
                .processor_work_dirs
                .iter()
                .any(|root| cwd == *root || cwd.starts_with(root))
        })
    }

    fn drop_processor_event(&self, event: &AgentEvent) -> bool {
        let Some(session_id) = event.session_id.as_deref() else {
            return false;
        };
        if !self.is_known_session_id(session_id) {
            return false;
        }
        let Ok(mut state) = self.inner.lock() else {
            warn!("processor session registry lock poisoned while dropping event");
            return true;
        };
        state.processor_events_dropped = state.processor_events_dropped.saturating_add(1);
        debug!(
            event_id = %event.id,
            session_id,
            source = %event.source,
            kind = ?event.kind,
            "processor session event dropped before feed ingest"
        );
        true
    }

    fn record_processor_session_skipped(&self, session_id: Option<&str>) {
        let Ok(mut state) = self.inner.lock() else {
            warn!("processor session registry lock poisoned while recording skipped session");
            return;
        };
        if let Some(session_id) = session_id.filter(|session_id| !session_id.trim().is_empty()) {
            let session_id = session_id.to_string();
            state.session_ids.insert(session_id.clone());
            if !state.skipped_session_ids.insert(session_id) {
                return;
            }
            state.processor_sessions_skipped = state.processor_sessions_skipped.saturating_add(1);
        } else {
            state.processor_sessions_skipped = state.processor_sessions_skipped.saturating_add(1);
            state.ambiguous_internal_candidates =
                state.ambiguous_internal_candidates.saturating_add(1);
        }
    }

    fn snapshot(&self) -> ProcessorSessionRegistrySnapshot {
        self.maybe_refresh();
        self.inner.lock().map_or_else(
            |_| {
                warn!("processor session registry lock poisoned while snapshotting");
                ProcessorSessionRegistrySnapshot::default()
            },
            |state| ProcessorSessionRegistrySnapshot {
                known_sessions: state.session_ids.len(),
                processor_events_dropped: state.processor_events_dropped,
                processor_sessions_skipped: state.processor_sessions_skipped,
                ambiguous_internal_candidates: state.ambiguous_internal_candidates,
            },
        )
    }
}

fn summary_memory_path(config: &SummaryConfig) -> Option<PathBuf> {
    match &config.processor {
        SummaryProcessorConfig::CodexSessionMemory { store_path, .. } => {
            Some(PathBuf::from(store_path))
        }
        _ => None,
    }
}

fn summary_memory_work_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("codex-memory-work")
}

fn summary_memory_codex_session_ids(path: &Path) -> Vec<String> {
    let Ok(input) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&input) else {
        warn!(
            path = %path.display(),
            "summary memory store could not be parsed while refreshing processor guard"
        );
        return Vec::new();
    };
    let mut session_ids = Vec::new();
    collect_summary_memory_codex_session_ids(&value, &mut session_ids);
    session_ids
}

fn collect_summary_memory_codex_session_ids(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "codex_session_id"
                    && let Some(session_id) = value.as_str()
                    && !session_id.trim().is_empty()
                {
                    output.push(session_id.to_string());
                    continue;
                }
                collect_summary_memory_codex_session_ids(value, output);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_summary_memory_codex_session_ids(value, output);
            }
        }
        _ => {}
    }
}

#[derive(Debug)]
struct ServeAgentCapture {
    agents: String,
    sessions: usize,
    workspace: Option<WorkspaceFilter>,
    include_history: bool,
    poll_ms: u64,
    codex_history: Option<PathBuf>,
    codex_sessions_dir: Option<PathBuf>,
    claude_projects_dir: Option<PathBuf>,
    event_sink: Option<mpsc::Sender<AgentEvent>>,
    processor_registry: ProcessorSessionRegistry,
}

struct ServePublishConfig {
    receiver: mpsc::Receiver<AgentEvent>,
    server: String,
    edge: String,
    network_id: String,
    feed: String,
    session: GithubAuthSession,
    publisher: PublisherIdentity,
    summary_config: SummaryConfig,
    publish_interval: Duration,
    edge_fallback: EdgeFallbackMode,
    processor_registry: ProcessorSessionRegistry,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct EdgePublishAck {
    #[serde(default)]
    accepted: usize,
    #[serde(default)]
    feeds: usize,
    #[serde(default)]
    headlines: usize,
}

#[derive(Clone, Debug, Default)]
struct PublishBatchResult {
    capsules: usize,
    edge_ack: Option<EdgePublishAck>,
}

#[derive(Clone, Copy, Debug, Default)]
struct PublishStatusPost<'a> {
    state: &'a str,
    pending_stories: usize,
    last_batch_stories: usize,
    last_batch_capsules: usize,
    edge_ack: Option<&'a EdgePublishAck>,
    detail: Option<&'a str>,
    last_error: Option<&'a str>,
}

fn start_serve_agent_capture(bind: SocketAddr, capture: ServeAgentCapture) {
    let selected_agents = parse_agent_list(&capture.agents);
    let capture_codex = selected_agents.contains("codex");
    let capture_claude = selected_agents.contains("claude");
    if !capture_codex && !capture_claude {
        warn!(
            agents = %capture.agents,
            "serve agent capture disabled because no supported agents were selected"
        );
        println!("agent capture disabled; supported agents are codex and claude");
        return;
    }

    let server = bind.to_string();
    let include_history = capture.include_history;
    if capture_codex {
        let server = server.clone();
        let history = capture.codex_history.unwrap_or_else(default_codex_history);
        let sessions_dir = capture
            .codex_sessions_dir
            .unwrap_or_else(default_codex_sessions_dir);
        let workspace = capture.workspace.clone();
        let sessions = capture.sessions;
        let poll_ms = capture.poll_ms;
        let event_sink = capture.event_sink.clone();
        let processor_registry = capture.processor_registry.clone();
        spawn_agent_capture("codex", move || {
            info!(
                sessions_requested = sessions,
                history = %history.display(),
                sessions_dir = %sessions_dir.display(),
                workspace = workspace.log_value(),
                include_history,
                %server,
                poll_ms,
                "serve codex active capture starting"
            );
            println!(
                "codex capture: watching future events for {}; use --include-history to replay selected history",
                capture_scope_message(workspace.as_ref())
            );
            watch_codex_active_sessions(CodexActiveWatch {
                server: &server,
                history: &history,
                sessions_dir: &sessions_dir,
                sessions,
                poll_ms,
                workspace: workspace.as_ref(),
                include_history,
                event_sink: event_sink.as_ref(),
                processor_registry,
            })
        });
    }

    if capture_claude {
        let server = server.clone();
        let projects_dir = capture
            .claude_projects_dir
            .unwrap_or_else(default_claude_projects_dir);
        let workspace = capture.workspace;
        let sessions = capture.sessions;
        let poll_ms = capture.poll_ms;
        let event_sink = capture.event_sink;
        spawn_agent_capture("claude", move || {
            info!(
                sessions_requested = sessions,
                projects_dir = %projects_dir.display(),
                workspace = workspace.log_value(),
                include_history,
                %server,
                poll_ms,
                "serve claude active capture starting"
            );
            println!(
                "claude capture: watching future events for {}; use --include-history to replay selected history",
                capture_scope_message(workspace.as_ref())
            );
            watch_claude_active_sessions(
                &server,
                &projects_dir,
                sessions,
                poll_ms,
                workspace.as_ref(),
                include_history,
                event_sink.as_ref(),
            )
        });
    }
}

fn spawn_agent_capture<F>(agent: &'static str, capture: F)
where
    F: FnOnce() -> Result<(), CliError> + Send + 'static,
{
    let result = std::thread::Builder::new()
        .name(format!("agent-feed-{agent}-capture"))
        .spawn(move || {
            if let Err(err) = capture() {
                error!(%agent, error = %err, "serve agent capture stopped");
                eprintln!("agent-feed: {agent} capture stopped: {err}");
            }
        });
    if let Err(err) = result {
        warn!(%agent, error = %err, "serve agent capture thread failed to start");
        eprintln!("agent-feed: failed to start {agent} capture: {err}");
    }
}

fn spawn_serve_publisher(config: ServePublishConfig) {
    if let Err(err) = std::thread::Builder::new()
        .name("agent-feed-p2p-publish".to_string())
        .spawn(move || run_serve_publisher(config))
    {
        warn!(error = %err, "serve p2p publisher thread failed to start");
        eprintln!("agent-feed: failed to start p2p publisher: {err}");
    }
}

fn run_serve_publisher(config: ServePublishConfig) {
    info!(
        feed_name = %config.feed,
        edge = %config.edge,
        network_id = %config.network_id,
        interval_ms = config.publish_interval.as_millis(),
        edge_fallback = config.edge_fallback.as_str(),
        data_plane = P2pDataPlane::EdgeSnapshotFallback.as_str(),
        "serve p2p publisher started"
    );
    println!(
        "p2p publish: signed in as @{}; publishing future summarized stories to `{}` via edge snapshot fallback",
        config.session.login, config.feed
    );
    post_serve_publish_status(
        &config,
        PublishStatusPost {
            state: "starting",
            detail: Some("publisher thread started"),
            ..PublishStatusPost::default()
        },
    );

    let mut compiler = StoryCompiler::default();
    let mut pending = Vec::<CompiledStory>::new();
    let mut recent = VecDeque::<RecentSummary>::new();
    let mut next_seq = 1u64;
    let mut last_publish = Instant::now();
    let mut last_flush = Instant::now();
    let mut last_presence = Instant::now() - Duration::from_secs(60);

    loop {
        match config.receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(event) => {
                if config.processor_registry.drop_processor_event(&event) {
                    continue;
                }
                debug!(
                    event_id = %event.id,
                    kind = ?event.kind,
                    agent = %event.agent,
                    project = event.project.as_deref().unwrap_or("local"),
                    session_id = event.session_id.as_deref().unwrap_or("<none>"),
                    turn_id = event.turn_id.as_deref().unwrap_or("<none>"),
                    score_hint = event.score_hint.unwrap_or_default(),
                    startup_context = event.tags.iter().any(|tag| tag == STARTUP_CONTEXT_TAG),
                    "p2p publisher received local event"
                );
                let stories = compiler.ingest(event);
                log_story_compiler_decision("ingest", &compiler.diagnostics());
                if !stories.is_empty() {
                    info!(
                        stories = stories.len(),
                        "p2p publisher queued story updates"
                    );
                    for story in &stories {
                        log_compiled_story("queued", story);
                    }
                    pending.extend(stories);
                    post_serve_publish_status(
                        &config,
                        PublishStatusPost {
                            state: "queued",
                            pending_stories: pending.len(),
                            detail: Some("story updates queued for publish"),
                            ..PublishStatusPost::default()
                        },
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                warn!("p2p publisher event channel disconnected");
                return;
            }
        }

        if last_presence.elapsed() >= Duration::from_secs(60) {
            match publish_feed_presence(&config) {
                Ok(ack) => {
                    last_presence = Instant::now();
                    info!(feed_name = %config.feed, "p2p feed presence registered");
                    post_serve_publish_status(
                        &config,
                        PublishStatusPost {
                            state: "present",
                            pending_stories: pending.len(),
                            edge_ack: Some(&ack),
                            detail: Some("feed presence registered at edge"),
                            ..PublishStatusPost::default()
                        },
                    );
                }
                Err(err) => {
                    warn!(
                        feed_name = %config.feed,
                        error = %err,
                        "p2p feed presence registration failed"
                    );
                    let error = err.to_string();
                    post_serve_publish_status(
                        &config,
                        PublishStatusPost {
                            state: "degraded",
                            pending_stories: pending.len(),
                            last_error: Some(&error),
                            ..PublishStatusPost::default()
                        },
                    );
                }
            }
        }

        if last_flush.elapsed() >= config.publish_interval {
            let flushed = compiler.flush();
            log_story_compiler_decision("flush", &compiler.diagnostics());
            last_flush = Instant::now();
            if !flushed.is_empty() {
                info!(
                    stories = flushed.len(),
                    "p2p publisher flushed story windows"
                );
                for story in &flushed {
                    log_compiled_story("flushed", story);
                }
                pending.extend(flushed);
                post_serve_publish_status(
                    &config,
                    PublishStatusPost {
                        state: "queued",
                        pending_stories: pending.len(),
                        detail: Some("story compiler flushed publishable windows"),
                        ..PublishStatusPost::default()
                    },
                );
            }
        }

        let should_publish = !pending.is_empty()
            && (last_publish.elapsed() >= config.publish_interval
                || pending.iter().any(|story| story.score >= 90)
                || pending.len() >= 8);
        if !should_publish {
            if !pending.is_empty() {
                debug!(
                    pending_stories = pending.len(),
                    elapsed_ms = last_publish.elapsed().as_millis(),
                    interval_ms = config.publish_interval.as_millis(),
                    high_priority = pending.iter().any(|story| story.score >= 90),
                    "p2p publish waiting for interval, priority, or batch size"
                );
            }
            continue;
        }

        let stories = std::mem::take(&mut pending);
        post_serve_publish_status(
            &config,
            PublishStatusPost {
                state: "publishing",
                last_batch_stories: stories.len(),
                detail: Some("sending summarized capsules to edge"),
                ..PublishStatusPost::default()
            },
        );
        match publish_story_batch(&config, &stories, &mut recent, &mut next_seq) {
            Ok(result) => {
                config.processor_registry.refresh();
                last_publish = Instant::now();
                if result.capsules > 0 {
                    println!(
                        "p2p publish: sent {} capsule(s) from {} story update(s)",
                        result.capsules,
                        stories.len()
                    );
                    post_serve_publish_status(
                        &config,
                        PublishStatusPost {
                            state: "published",
                            last_batch_stories: stories.len(),
                            last_batch_capsules: result.capsules,
                            edge_ack: result.edge_ack.as_ref(),
                            detail: Some("edge accepted story capsule batch"),
                            ..PublishStatusPost::default()
                        },
                    );
                } else {
                    println!(
                        "p2p publish: skipped {} story update(s); no meaningful headline change",
                        stories.len()
                    );
                    post_serve_publish_status(
                        &config,
                        PublishStatusPost {
                            state: "skipped",
                            last_batch_stories: stories.len(),
                            edge_ack: result.edge_ack.as_ref(),
                            detail: Some("no meaningful headline change"),
                            ..PublishStatusPost::default()
                        },
                    );
                }
            }
            Err(err) => {
                warn!(
                    feed_name = %config.feed,
                    stories = stories.len(),
                    error = %err,
                    "p2p publish batch failed"
                );
                eprintln!("agent-feed: p2p publish failed: {err}");
                pending.extend(stories);
                let error = err.to_string();
                post_serve_publish_status(
                    &config,
                    PublishStatusPost {
                        state: "error",
                        pending_stories: pending.len(),
                        last_error: Some(&error),
                        ..PublishStatusPost::default()
                    },
                );
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

fn publish_feed_presence(config: &ServePublishConfig) -> Result<EdgePublishAck, CliError> {
    let feed_id = publish_feed_id(config);
    let body = serde_json::to_string(&json!({
        "network_id": config.network_id,
        "compatibility": ProtocolCompatibility::current(),
        "feed_id": feed_id,
        "feed_name": config.feed,
        "publisher": config.publisher,
        "capsules": [],
    }))?;
    let response =
        post_edge_json_with_bearer(&config.edge, "/network/publish", &body, &config.session)?;
    let ack = parse_edge_publish_ack(&response)?;
    debug!(
        feed_name = %config.feed,
        response = %response.trim(),
        "p2p feed presence sent to edge"
    );
    Ok(ack)
}

fn publish_story_batch(
    config: &ServePublishConfig,
    stories: &[CompiledStory],
    recent: &mut VecDeque<RecentSummary>,
    next_seq: &mut u64,
) -> Result<PublishBatchResult, CliError> {
    let feed_id = publish_feed_id(config);
    let recent_summaries = recent.iter().cloned().collect::<Vec<_>>();
    info!(
        feed_name = %config.feed,
        %feed_id,
        stories = stories.len(),
        recent_summaries = recent_summaries.len(),
        summary_mode = ?config.summary_config.mode,
        processor = summary_processor_label(&config.summary_config),
        publish_policy_enabled = config.summary_config.publish.enabled,
        recent_window = config.summary_config.publish.recent_window,
        "p2p publish summarization started"
    );
    let summaries =
        summarize_feed_with_recent(&feed_id, stories, &config.summary_config, &recent_summaries)?;
    info!(
        feed_name = %config.feed,
        %feed_id,
        stories = stories.len(),
        summaries = summaries.len(),
        "p2p publish summarization completed"
    );
    for summary in &summaries {
        info!(
            feed_name = %config.feed,
            %feed_id,
            action = summary.metadata.publish_action.as_str(),
            reason = %summary.metadata.publish_reason,
            processor = %summary.metadata.processor,
            story_family = ?summary.story_family,
            score = summary.score,
            input_stories = summary.metadata.input_stories,
            output_chars = summary.metadata.output_chars,
            headline_similarity = summary.metadata.max_headline_similarity,
            deck_similarity = summary.metadata.max_deck_similarity,
            update_relation = %summary.metadata.update_relation,
            topic_fingerprint = %summary.metadata.topic_fingerprint,
            state_fingerprint = %summary.metadata.state_fingerprint,
            impact_fingerprint = %summary.metadata.impact_fingerprint,
            violations = summary.metadata.violations.len(),
            headline_fingerprint = %summary.metadata.headline_fingerprint,
            "p2p publish summary accepted for capsule"
        );
    }
    for summary in &summaries {
        recent.push_front(RecentSummary::from(summary));
    }
    while recent.len() > config.summary_config.publish.recent_window.max(1) {
        recent.pop_back();
    }

    let capsules = signed_capsules_from_summaries_with_feed_id(
        &feed_id,
        &summaries,
        Some(&config.publisher),
        config.session.session_token.as_deref(),
        *next_seq,
    )?;
    *next_seq += capsules.len() as u64;

    if capsules.is_empty() {
        info!(
            feed_name = %config.feed,
            %feed_id,
            stories = stories.len(),
            summaries = summaries.len(),
            "p2p publish skipped empty capsule batch"
        );
        return Ok(PublishBatchResult {
            capsules: 0,
            edge_ack: None,
        });
    }

    let body = serde_json::to_string(&json!({
        "network_id": config.network_id,
        "compatibility": ProtocolCompatibility::current(),
        "feed_id": feed_id,
        "feed_name": config.feed,
        "publisher": config.publisher,
        "capsules": capsules,
    }))?;
    let response =
        post_edge_json_with_bearer(&config.edge, "/network/publish", &body, &config.session)?;
    let ack = parse_edge_publish_ack(&response)?;
    info!(
        feed_name = %config.feed,
        capsules = capsules.len(),
        stories = stories.len(),
        response = %response.trim(),
        "p2p publish sent to edge"
    );
    Ok(PublishBatchResult {
        capsules: capsules.len(),
        edge_ack: Some(ack),
    })
}

fn log_story_compiler_decision(scope: &'static str, diagnostics: &StoryCompilerDiagnostics) {
    let Some(decision) = diagnostics.last_decision.as_ref() else {
        return;
    };
    match decision.action {
        StoryDecisionAction::Published
        | StoryDecisionAction::Rejected
        | StoryDecisionAction::Deduped => log_story_decision_info(scope, decision, diagnostics),
        StoryDecisionAction::Retained => {
            debug!(
                scope,
                action = story_decision_action(decision.action),
                reason = %decision.reason,
                agent = %decision.agent,
                family = ?decision.family,
                score = decision.score,
                context_score = decision.context_score,
                open_windows = diagnostics.open_windows,
                retained_windows = diagnostics.retained_windows,
                "story compiler retained a window"
            );
        }
        StoryDecisionAction::Waiting => {
            debug!(
                scope,
                action = story_decision_action(decision.action),
                reason = %decision.reason,
                agent = %decision.agent,
                family = ?decision.family,
                score = decision.score,
                context_score = decision.context_score,
                open_windows = diagnostics.open_windows,
                "story compiler waiting"
            );
        }
    }
}

fn log_story_decision_info(
    scope: &'static str,
    decision: &StoryDecision,
    diagnostics: &StoryCompilerDiagnostics,
) {
    info!(
        scope,
        action = story_decision_action(decision.action),
        reason = %decision.reason,
        agent = %decision.agent,
        family = ?decision.family,
        score = decision.score,
        context_score = decision.context_score,
        open_windows = diagnostics.open_windows,
        settled_windows = diagnostics.settled_windows,
        published_stories = diagnostics.published_stories,
        rejected_stories = diagnostics.rejected_stories,
        deduped_stories = diagnostics.deduped_stories,
        "story compiler decision"
    );
}

fn log_compiled_story(stage: &'static str, story: &CompiledStory) {
    info!(
        stage,
        agent = %story.agent,
        project = story.project.as_deref().unwrap_or("local"),
        family = ?story.family,
        score = story.score,
        context_score = story.context_score,
        severity = ?story.severity,
        evidence_events = story.evidence_event_ids.len(),
        headline_words = story.headline.split_whitespace().count(),
        deck_words = story.deck.split_whitespace().count(),
        "story queued for p2p summarization"
    );
}

fn story_decision_action(action: StoryDecisionAction) -> &'static str {
    match action {
        StoryDecisionAction::Waiting => "waiting",
        StoryDecisionAction::Retained => "retained",
        StoryDecisionAction::Rejected => "rejected",
        StoryDecisionAction::Deduped => "deduped",
        StoryDecisionAction::Published => "published",
    }
}

fn summary_processor_label(config: &SummaryConfig) -> &'static str {
    match &config.processor {
        SummaryProcessorConfig::Deterministic => "deterministic",
        SummaryProcessorConfig::CodexExec => "codex-exec",
        SummaryProcessorConfig::CodexSessionMemory { .. } => "codex-memory",
        SummaryProcessorConfig::ClaudeCodeExec => "claude-code",
        SummaryProcessorConfig::Process { .. } => "process",
        SummaryProcessorConfig::HttpEndpoint { .. } => "http-endpoint",
    }
}

fn parse_edge_publish_ack(response: &str) -> Result<EdgePublishAck, CliError> {
    serde_json::from_str(response.trim()).map_err(CliError::from)
}

fn post_serve_publish_status(config: &ServePublishConfig, status: PublishStatusPost<'_>) {
    let processor = config.processor_registry.snapshot();
    let body = match serde_json::to_string(&PublishStatusUpdate {
        feed: config.feed.clone(),
        state: status.state.to_string(),
        edge: config.edge.clone(),
        network_id: config.network_id.clone(),
        publisher: Some(format!("@{}", config.session.login)),
        pending_stories: status.pending_stories,
        last_batch_stories: status.last_batch_stories,
        last_batch_capsules: status.last_batch_capsules,
        last_edge_accepted: status.edge_ack.map_or(0, |ack| ack.accepted),
        last_edge_feeds: status.edge_ack.map_or(0, |ack| ack.feeds),
        last_edge_headlines: status.edge_ack.map_or(0, |ack| ack.headlines),
        processor_sessions: processor.known_sessions,
        processor_events_dropped: processor.processor_events_dropped,
        processor_sessions_skipped: processor.processor_sessions_skipped,
        ambiguous_internal_candidates: processor.ambiguous_internal_candidates,
        detail: status.detail.map(str::to_string),
        last_error: status.last_error.map(str::to_string),
    }) {
        Ok(body) => body,
        Err(err) => {
            warn!(error = %err, "publish status serialization failed");
            return;
        }
    };
    if let Err(err) = post_json(&config.server, "/publish/status", &body) {
        debug!(error = %err, "publish status post failed");
    }
}

fn local_feed_id(feed: &str) -> String {
    format!("local:{feed}")
}

fn publish_feed_id(config: &ServePublishConfig) -> String {
    match config.publisher.github_user_id {
        Some(github_user_id) => format!("github:{github_user_id}:{}", config.feed),
        None => local_feed_id(&config.feed),
    }
}

fn capture_scope_message(workspace: Option<&WorkspaceFilter>) -> String {
    workspace
        .map(|workspace| format!("workspace {}", workspace.display()))
        .unwrap_or_else(|| "all workspaces".to_string())
}

#[derive(Clone, Copy, Debug, Default)]
struct ImportStats {
    imported: usize,
    filtered: usize,
}

impl ImportStats {
    fn add(&mut self, other: Self) {
        self.imported += other.imported;
        self.filtered += other.filtered;
    }
}

#[derive(Debug, Default)]
struct CollectedEvents {
    events: Vec<AgentEvent>,
    filtered: usize,
}

fn event_matches_workspace(
    event: &AgentEvent,
    workspace: Option<&WorkspaceFilter>,
    path: &Path,
    source: &str,
) -> bool {
    let Some(workspace) = workspace else {
        return true;
    };
    if workspace.allows(event) {
        return true;
    }
    debug!(
        %source,
        path = %path.display(),
        event_id = %event.id,
        cwd = event.cwd.as_deref().unwrap_or("<none>"),
        workspace = %workspace.display(),
        "agent event dropped by workspace filter"
    );
    false
}

fn normalize_absolute_path(path: PathBuf) -> Result<PathBuf, CliError> {
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(clean_path(&absolute))
}

fn expand_home_path(path: PathBuf) -> PathBuf {
    let Some(raw) = path.to_str().map(ToOwned::to_owned) else {
        return path;
    };
    if raw == "~" {
        return home_dir().unwrap_or(path);
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    path
}

fn clean_path(path: &Path) -> PathBuf {
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                cleaned.pop();
            }
            std::path::Component::Normal(part) => cleaned.push(part),
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                cleaned.push(component.as_os_str());
            }
        }
    }
    cleaned
}

fn print_import_complete(label: &str, stats: ImportStats) {
    if stats.filtered == 0 {
        println!("{label} import complete: {} events", stats.imported);
    } else {
        println!(
            "{label} import complete: {} events; {} filtered outside workspace",
            stats.imported, stats.filtered
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_story::StoryFamily;
    use agent_feed_summarize::{PublishAction, SummaryMetadata};

    fn temp_test_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("agent-feed-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp root creates");
        root
    }

    fn write_jsonl(path: &Path, values: impl IntoIterator<Item = Value>) {
        let lines = values
            .into_iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(path, format!("{lines}\n")).expect("jsonl writes");
    }

    fn internal_story_summary_prompt() -> String {
        format!(
            "You are the private local headline memory for one agent feed. Maintain continuity across calls, but publish only when the new redacted delta changes the public story.\nsummary_memory_key=feed:test:workspace:all-workspaces\nprior_memory_digest=none\nprior_semantic_fingerprint=none\n\n{INTERNAL_SUMMARIZER_MARKER}\nReturn one JSON object with headline, deck, lower_third, chips, and optional publish/publish_reason. Set publish=false when the candidate is not meaningfully different from recent published summaries.\nfeed=github:1:workstation\nmode=FeedRollup\nrecent_published=none\nstories:\n- agent=codex family=Turn score=76 headline=codex checked ci status deck=16 safe command events settled around ci status."
        )
    }

    #[test]
    fn p2p_publish_accepts_feed_name_alias() {
        let cli = Cli::try_parse_from([
            "agent-feed",
            "p2p",
            "publish",
            "--dry-run",
            "--feed-name",
            "gpu-vm",
        ])
        .expect("feed-name alias parses");

        let Commands::P2p {
            command: P2pCommand::Publish { feed, .. },
        } = cli.command
        else {
            panic!("expected p2p publish command");
        };
        assert_eq!(feed, "gpu-vm");
    }

    #[test]
    fn codex_active_accepts_workspace_scope() {
        let cli = Cli::try_parse_from([
            "agent-feed",
            "codex",
            "active",
            "--workspace",
            "/tmp/agent-feed-workspace",
        ])
        .expect("workspace scope parses");

        let Commands::Codex {
            command: CodexCommand::Active { workspace, .. },
        } = cli.command
        else {
            panic!("expected codex active command");
        };
        assert_eq!(workspace, Some(PathBuf::from("/tmp/agent-feed-workspace")));
    }

    #[test]
    fn p2p_publish_accepts_workspace_scope() {
        let cli = Cli::try_parse_from([
            "agent-feed",
            "p2p",
            "publish",
            "--dry-run",
            "--workspace",
            "/tmp/agent-feed-workspace",
        ])
        .expect("workspace scope parses");

        let Commands::P2p {
            command: P2pCommand::Publish { workspace, .. },
        } = cli.command
        else {
            panic!("expected p2p publish command");
        };
        assert_eq!(workspace, Some(PathBuf::from("/tmp/agent-feed-workspace")));
    }

    #[test]
    fn p2p_publish_carries_network_id() {
        let cli = Cli::try_parse_from([
            "agent-feed",
            "p2p",
            "publish",
            "--dry-run",
            "--network-id",
            "agent-feed-lab",
            "--edge-fallback",
            "on",
        ])
        .expect("network id parses");

        let Commands::P2p {
            command:
                P2pCommand::Publish {
                    network_id,
                    edge_fallback,
                    ..
                },
        } = cli.command
        else {
            panic!("expected p2p publish command");
        };
        assert_eq!(network_id, "agent-feed-lab");
        assert_eq!(edge_fallback, CliEdgeFallback::On);
    }

    #[test]
    fn signed_publish_capsules_use_github_feed_id_when_authenticated() {
        let publisher = PublisherIdentity::github(
            35904762,
            "mosure".to_string(),
            Some("mitchell mosure".to_string()),
            Some("https://api.feed.aberration.technology/avatar/github/35904762".to_string()),
        );
        let summary = FeedSummary {
            story_window: "feed-rollup".to_string(),
            source_agent_kinds: vec!["codex".to_string()],
            headline: "codex advanced the feed publisher".to_string(),
            deck: "capture and summarization produced a p2p-safe story.".to_string(),
            lower_third: "@mosure / workstation".to_string(),
            chips: vec!["codex".to_string(), "publish".to_string()],
            image: None,
            story_family: StoryFamily::IdleRecap,
            severity: agent_feed_core::Severity::Notice,
            score: 88,
            privacy_class: agent_feed_core::PrivacyClass::Redacted,
            evidence_event_ids: Vec::new(),
            metadata: SummaryMetadata {
                processor: "test".to_string(),
                policy: "p2p-strict".to_string(),
                image_processor: "disabled".to_string(),
                publish_action: PublishAction::Publish,
                publish_reason: "test accepted".to_string(),
                headline_fingerprint: "codex:feed:publisher".to_string(),
                topic_fingerprint: "feed:publisher".to_string(),
                state_fingerprint: "implemented".to_string(),
                impact_fingerprint: "network".to_string(),
                update_relation: "new_topic".to_string(),
                max_headline_similarity: 0,
                max_deck_similarity: 0,
                guardrail_version: 1,
                input_stories: 1,
                output_chars: 64,
                external_cost_allowed: false,
                image_enabled: false,
                violations: Vec::new(),
            },
        };

        let capsules =
            signed_capsules_from_summaries("workstation", &[summary], Some(&publisher), None, 1)
                .expect("capsule signs");

        assert_eq!(capsules[0].value.feed_id, "github:35904762:workstation");
        assert_eq!(
            capsules[0]
                .value
                .publisher
                .as_ref()
                .and_then(|publisher| publisher.github_login.as_deref()),
            Some("mosure")
        );
    }

    #[test]
    fn p2p_publish_defaults_to_aesthetic_headline_summarizer() {
        let cli = Cli::try_parse_from(["agent-feed", "p2p", "publish", "--dry-run"])
            .expect("publish command parses");

        let Commands::P2p {
            command:
                P2pCommand::Publish {
                    summarizer,
                    summary_style,
                    summary_prompt_max_chars,
                    include_history,
                    ..
                },
        } = cli.command
        else {
            panic!("expected p2p publish command");
        };
        assert_eq!(summarizer, "codex-memory");
        assert_eq!(summary_style, DEFAULT_SUMMARY_PROMPT_STYLE);
        assert_eq!(summary_prompt_max_chars, DEFAULT_SUMMARY_PROMPT_MAX_CHARS);
        assert!(!include_history);
    }

    #[test]
    fn summary_config_routes_codex_memory_processor() {
        let store = PathBuf::from("/tmp/agent-feed-summary-memory-test.json");
        let mut config = summary_config(SummaryCliOptions {
            summarizer: "codex-memory",
            summary_style: DEFAULT_SUMMARY_PROMPT_STYLE,
            summary_prompt_max_chars: DEFAULT_SUMMARY_PROMPT_MAX_CHARS,
            per_story: false,
            allow_project_names: false,
            summary_memory_store: Some(&store),
            summary_endpoint: None,
            summary_auth_header_env: None,
            summary_command: None,
            summary_args: &[],
            guardrail_patterns: &[],
            images: false,
            image_processor: "codex-exec",
            image_endpoint: None,
            image_command: None,
            image_args: &[],
            image_style: None,
            image_prompt_max_chars: None,
            allow_remote_image_urls: false,
        })
        .expect("summary config builds");
        let selected = parse_agent_list("codex,claude");
        scope_summary_memory(&mut config, "workstation", &selected, None, false)
            .expect("memory scopes");

        let SummaryProcessorConfig::CodexSessionMemory {
            store_path,
            key,
            command,
        } = config.processor
        else {
            panic!("expected codex memory processor");
        };
        assert_eq!(store_path, store.display().to_string());
        assert!(key.contains("feed:workstation"));
        assert!(key.contains("agents:claude+codex"));
        assert_eq!(command, default_codex_command());
    }

    #[test]
    fn summary_config_routes_cli_processors_and_keeps_strict_defaults() {
        let guardrail_patterns = vec![r"(?i)customer-name".to_string()];
        let config = summary_config(SummaryCliOptions {
            summarizer: "claude-code",
            summary_style: "quiet feed style",
            summary_prompt_max_chars: 128,
            per_story: true,
            allow_project_names: false,
            summary_memory_store: None,
            summary_endpoint: None,
            summary_auth_header_env: None,
            summary_command: None,
            summary_args: &[],
            guardrail_patterns: &guardrail_patterns,
            images: false,
            image_processor: "http-endpoint",
            image_endpoint: None,
            image_command: None,
            image_args: &[],
            image_style: None,
            image_prompt_max_chars: None,
            allow_remote_image_urls: false,
        })
        .expect("summary config builds");

        assert_eq!(config.mode, FeedSummaryMode::PerStory);
        assert_eq!(config.processor, SummaryProcessorConfig::ClaudeCodeExec);
        assert_eq!(config.prompt.style, "quiet feed style");
        assert_eq!(config.prompt.max_prompt_chars, 512);
        assert!(!config.guardrails.allow_project_names);
        assert!(!config.guardrails.allow_command_text);
        assert!(config.guardrails.patterns.iter().any(|pattern| {
            pattern.name == "cli-guardrail-0" && pattern.pattern == "(?i)customer-name"
        }));
        assert!(!config.image.enabled);
        assert_eq!(config.image.processor, ImageProcessorConfig::Disabled);
    }

    #[test]
    fn summary_config_exposes_external_summary_processors() {
        let command_config = summary_config(SummaryCliOptions {
            summarizer: "process",
            summary_style: DEFAULT_SUMMARY_PROMPT_STYLE,
            summary_prompt_max_chars: DEFAULT_SUMMARY_PROMPT_MAX_CHARS,
            per_story: false,
            allow_project_names: false,
            summary_memory_store: None,
            summary_endpoint: None,
            summary_auth_header_env: None,
            summary_command: Some("summarize-feed"),
            summary_args: &["--json".to_string()],
            guardrail_patterns: &[],
            images: false,
            image_processor: "codex-exec",
            image_endpoint: None,
            image_command: None,
            image_args: &[],
            image_style: None,
            image_prompt_max_chars: None,
            allow_remote_image_urls: false,
        })
        .expect("process summarizer config builds");

        assert_eq!(
            command_config.processor,
            SummaryProcessorConfig::Process {
                command: "summarize-feed".to_string(),
                args: vec!["--json".to_string()],
            }
        );

        let endpoint_config = summary_config(SummaryCliOptions {
            summarizer: "http-endpoint",
            summary_style: DEFAULT_SUMMARY_PROMPT_STYLE,
            summary_prompt_max_chars: DEFAULT_SUMMARY_PROMPT_MAX_CHARS,
            per_story: false,
            allow_project_names: false,
            summary_memory_store: None,
            summary_endpoint: Some("http://127.0.0.1:8181/summarize"),
            summary_auth_header_env: Some("FEED_SUMMARY_AUTH"),
            summary_command: None,
            summary_args: &[],
            guardrail_patterns: &[],
            images: false,
            image_processor: "codex-exec",
            image_endpoint: None,
            image_command: None,
            image_args: &[],
            image_style: None,
            image_prompt_max_chars: None,
            allow_remote_image_urls: false,
        })
        .expect("http summarizer config builds");

        assert_eq!(
            endpoint_config.processor,
            SummaryProcessorConfig::HttpEndpoint {
                url: "http://127.0.0.1:8181/summarize".to_string(),
                auth_header_env: Some("FEED_SUMMARY_AUTH".to_string()),
            }
        );
    }

    #[test]
    fn summary_config_rejects_unknown_routes_and_unsafe_image_config() {
        let unknown = summary_config(SummaryCliOptions {
            summarizer: "random-llm",
            summary_style: DEFAULT_SUMMARY_PROMPT_STYLE,
            summary_prompt_max_chars: DEFAULT_SUMMARY_PROMPT_MAX_CHARS,
            per_story: false,
            allow_project_names: false,
            summary_memory_store: None,
            summary_endpoint: None,
            summary_auth_header_env: None,
            summary_command: None,
            summary_args: &[],
            guardrail_patterns: &[],
            images: false,
            image_processor: "codex-exec",
            image_endpoint: None,
            image_command: None,
            image_args: &[],
            image_style: None,
            image_prompt_max_chars: None,
            allow_remote_image_urls: false,
        })
        .expect_err("unknown summarizer is rejected");
        assert!(unknown.to_string().contains("unknown summarizer"));

        let missing_endpoint = summary_config(SummaryCliOptions {
            summarizer: "codex-exec",
            summary_style: DEFAULT_SUMMARY_PROMPT_STYLE,
            summary_prompt_max_chars: DEFAULT_SUMMARY_PROMPT_MAX_CHARS,
            per_story: false,
            allow_project_names: false,
            summary_memory_store: None,
            summary_endpoint: None,
            summary_auth_header_env: None,
            summary_command: None,
            summary_args: &[],
            guardrail_patterns: &[],
            images: true,
            image_processor: "http-endpoint",
            image_endpoint: None,
            image_command: None,
            image_args: &[],
            image_style: None,
            image_prompt_max_chars: None,
            allow_remote_image_urls: false,
        })
        .expect_err("http image processor requires endpoint");
        assert!(
            missing_endpoint
                .to_string()
                .contains("--image-endpoint is required")
        );

        let missing_summary_endpoint = summary_config(SummaryCliOptions {
            summarizer: "http-endpoint",
            summary_style: DEFAULT_SUMMARY_PROMPT_STYLE,
            summary_prompt_max_chars: DEFAULT_SUMMARY_PROMPT_MAX_CHARS,
            per_story: false,
            allow_project_names: false,
            summary_memory_store: None,
            summary_endpoint: None,
            summary_auth_header_env: None,
            summary_command: None,
            summary_args: &[],
            guardrail_patterns: &[],
            images: false,
            image_processor: "codex-exec",
            image_endpoint: None,
            image_command: None,
            image_args: &[],
            image_style: None,
            image_prompt_max_chars: None,
            allow_remote_image_urls: false,
        })
        .expect_err("http summary processor requires endpoint");
        assert!(
            missing_summary_endpoint
                .to_string()
                .contains("--summary-endpoint is required")
        );
    }

    #[test]
    fn p2p_share_rejects_invalid_feed_name_alias() {
        let error = Cli::try_parse_from(["agent-feed", "p2p", "share", "--feed-name", ".env"])
            .expect_err("invalid feed label is rejected");

        assert!(error.to_string().contains("invalid feed label"));
    }

    #[test]
    fn global_logging_options_parse_for_subcommands() {
        let cli = Cli::try_parse_from([
            "agent-feed",
            "--log-level",
            "debug",
            "--log-format",
            "json",
            "p2p",
            "status",
        ])
        .expect("logging flags parse");

        assert_eq!(cli.log_level, "debug");
        assert_eq!(cli.log_format, LogFormat::Json);
    }

    #[test]
    fn serve_defaults_to_active_agent_capture() {
        let cli = Cli::try_parse_from(["agent-feed", "serve"]).expect("serve parses");

        let Commands::Serve {
            agents,
            sessions,
            no_agent_capture,
            workspace,
            all_workspaces,
            include_history,
            ..
        } = cli.command
        else {
            panic!("expected serve command");
        };

        assert_eq!(agents, "codex,claude");
        assert_eq!(sessions, 12);
        assert!(!no_agent_capture);
        assert_eq!(workspace, Some(PathBuf::from(".")));
        assert!(!all_workspaces);
        assert!(!include_history);
    }

    #[test]
    fn serve_can_disable_agent_capture_and_scope_workspace() {
        let cli = Cli::try_parse_from([
            "agent-feed",
            "serve",
            "--no-agent-capture",
            "--all-workspaces",
            "--agents",
            "codex",
            "--sessions",
            "8",
            "--workspace",
            "/tmp/agent-feed-workspace",
        ])
        .expect("serve capture flags parse");

        let Commands::Serve {
            agents,
            sessions,
            no_agent_capture,
            workspace,
            all_workspaces,
            include_history,
            ..
        } = cli.command
        else {
            panic!("expected serve command");
        };

        assert_eq!(agents, "codex");
        assert_eq!(sessions, 8);
        assert!(no_agent_capture);
        assert_eq!(workspace, Some(PathBuf::from("/tmp/agent-feed-workspace")));
        assert!(all_workspaces);
        assert!(!include_history);
    }

    #[test]
    fn serve_publish_parses_all_in_one_feed_options() {
        let cli = Cli::try_parse_from([
            "agent-feed",
            "serve",
            "--publish",
            "--all-workspaces",
            "--feed-name",
            "gpu-vm",
            "--publish-interval-secs",
            "5",
            "--summarizer",
            "deterministic",
            "--edge-fallback",
            "auto",
        ])
        .expect("serve publish flags parse");

        let Commands::Serve {
            publish,
            p2p,
            feed,
            all_workspaces,
            publish_interval_secs,
            summarizer,
            edge_fallback,
            ..
        } = cli.command
        else {
            panic!("expected serve command");
        };

        assert!(publish);
        assert!(!p2p);
        assert_eq!(feed, "gpu-vm");
        assert!(all_workspaces);
        assert_eq!(publish_interval_secs, 5);
        assert_eq!(summarizer, "deterministic");
        assert_eq!(edge_fallback, CliEdgeFallback::Auto);
    }

    #[test]
    fn status_command_defaults_to_loopback_and_supports_json() {
        let cli = Cli::try_parse_from(["agent-feed", "status", "--json"]).expect("status parses");

        let Commands::Status { server, json } = cli.command else {
            panic!("expected status command");
        };

        assert_eq!(server, LOOPBACK_ADDR);
        assert!(json);
    }

    #[test]
    fn doctor_publish_command_defaults_to_loopback_and_mainnet() {
        let cli = Cli::try_parse_from(["agent-feed", "doctor", "publish", "--json"])
            .expect("doctor publish parses");

        let Commands::Doctor {
            command:
                Some(DoctorCommand::Publish {
                    server,
                    edge,
                    network_id,
                    json,
                    ..
                }),
        } = cli.command
        else {
            panic!("expected doctor publish command");
        };

        assert_eq!(server, LOOPBACK_ADDR);
        assert_eq!(edge, "https://api.feed.aberration.technology");
        assert_eq!(network_id, "agent-feed-mainnet");
        assert!(json);
    }

    #[test]
    fn p2p_capability_json_exposes_transport_boundary() {
        let capability =
            agent_feed_p2p::P2pDataPlane::NativeLibp2p.capability(EdgeFallbackMode::Auto);
        let value = p2p_capability_json(&capability);

        assert_eq!(value["data_plane"], "native_libp2p");
        assert_eq!(value["available"], false);
        assert!(
            value["protocols"]
                .as_array()
                .expect("protocols array")
                .iter()
                .any(|protocol| protocol == "gossipsub")
        );
        assert!(
            value["transports"]
                .as_array()
                .expect("transports array")
                .iter()
                .any(|transport| transport == "webrtc_direct")
        );
    }

    #[test]
    fn publish_doctor_report_is_actionable_and_lowercase() {
        let report = PublishDoctorReport {
            status: DoctorCheckStatus::Warn,
            checks: vec![
                PublishDoctorCheck {
                    name: "auth",
                    status: DoctorCheckStatus::Ok,
                    detail: "signed in as @mosure".to_string(),
                },
                PublishDoctorCheck {
                    name: "story gate",
                    status: DoctorCheckStatus::Warn,
                    detail: "no events observed since this feed process started".to_string(),
                },
            ],
            next: "continue or restart codex/claude after `agent-feed serve --publish` is running"
                .to_string(),
        };

        let output = format_publish_doctor_report(&report);

        assert!(output.contains("publish doctor: warn"));
        assert!(output.contains("auth: ok · signed in as @mosure"));
        assert!(output.contains("story gate: warn"));
        assert!(output.contains("next: continue or restart codex/claude"));
    }

    #[test]
    fn local_status_explains_capture_watcher_state() {
        let now = time::OffsetDateTime::now_utc();
        let status = StatusView {
            status: "ok".to_string(),
            bind: LOOPBACK_ADDR.to_string(),
            p2p_enabled: true,
            ingested_events: 0,
            emitted_bulletins: 0,
            dropped_events: 0,
            stored_events: 0,
            stored_bulletins: 0,
            story: agent_feed_views::StoryStatusView::default(),
            publish: None,
            captured_sources: Vec::new(),
            capture_watchers: vec![agent_feed_views::CaptureWatchView {
                agent: "codex".to_string(),
                adapter: "codex.transcript".to_string(),
                label: "private.jsonl".to_string(),
                state: "watching".to_string(),
                workspace: Some("all".to_string()),
                session_id: Some("session".to_string()),
                last_append_ms: Some(1_000),
                offset: 42,
                file_len: 42,
                imported_events: 0,
                filtered_events: 0,
                poll_ms: 1000,
                updated_at: now,
            }],
            last_event_kind: None,
            last_event_at: None,
            last_bulletin_at: None,
        };

        let output = format_local_status(&status);

        assert!(output.contains("feed status: ok"));
        assert!(output.contains("p2p: enabled"));
        assert!(output.contains("capture: 1 watcher"));
        assert!(output.contains("codex codex.transcript watching"));
        assert!(output.contains("transcript watchers are live"));
        assert!(!output.contains("private.jsonl"));
    }

    #[test]
    fn local_status_explains_story_gate_state() {
        let now = time::OffsetDateTime::now_utc();
        let status = StatusView {
            status: "ok".to_string(),
            bind: LOOPBACK_ADDR.to_string(),
            p2p_enabled: true,
            ingested_events: 3,
            emitted_bulletins: 0,
            dropped_events: 0,
            stored_events: 3,
            stored_bulletins: 0,
            story: agent_feed_views::StoryStatusView {
                open_windows: 1,
                retained_windows: 0,
                settled_windows: 1,
                published_stories: 0,
                rejected_stories: 1,
                deduped_stories: 0,
                last_decision: Some(agent_feed_views::StoryDecisionView {
                    at: now,
                    action: "rejected".to_string(),
                    reason: "summary was too generic or mechanical to publish".to_string(),
                    agent: "codex".to_string(),
                    project: Some("agent_feed".to_string()),
                    session_id: Some("session".to_string()),
                    turn_id: Some("turn".to_string()),
                    family: "incident".to_string(),
                    score: 84,
                    context_score: 82,
                }),
                recent_decisions: Vec::new(),
            },
            publish: None,
            captured_sources: vec![agent_feed_views::CapturedSourceView {
                source: "codex".to_string(),
                agent: "codex".to_string(),
                adapter: "codex.transcript".to_string(),
                events: 3,
                sessions: 1,
                last_event_kind: "tool.fail".to_string(),
                last_event_at: now,
            }],
            capture_watchers: Vec::new(),
            last_event_kind: Some("tool.fail".to_string()),
            last_event_at: Some(now),
            last_bulletin_at: None,
        };

        let output = format_local_status(&status);

        assert!(output.contains("story gate: 1 open"));
        assert!(output.contains("last gate: codex rejected"));
        assert!(output.contains("summary was too generic or mechanical to publish"));
        assert!(output.contains("latest story gate is `rejected`"));
    }

    #[test]
    fn local_status_explains_publish_state() {
        let now = time::OffsetDateTime::now_utc();
        let status = StatusView {
            status: "ok".to_string(),
            bind: LOOPBACK_ADDR.to_string(),
            p2p_enabled: true,
            ingested_events: 4,
            emitted_bulletins: 1,
            dropped_events: 0,
            stored_events: 4,
            stored_bulletins: 1,
            story: agent_feed_views::StoryStatusView::default(),
            publish: Some(agent_feed_views::PublishStatusView {
                feed: "workstation".to_string(),
                state: "published".to_string(),
                edge: "https://api.feed.aberration.technology".to_string(),
                network_id: "agent-feed-mainnet".to_string(),
                publisher: Some("@mosure".to_string()),
                pending_stories: 0,
                last_batch_stories: 1,
                last_batch_capsules: 1,
                last_edge_accepted: 1,
                last_edge_feeds: 1,
                last_edge_headlines: 2,
                processor_sessions: 0,
                processor_events_dropped: 0,
                processor_sessions_skipped: 0,
                ambiguous_internal_candidates: 0,
                detail: Some("edge accepted story capsule batch".to_string()),
                last_error: None,
                updated_at: now,
            }),
            captured_sources: Vec::new(),
            capture_watchers: Vec::new(),
            last_event_kind: Some("turn.complete".to_string()),
            last_event_at: Some(now),
            last_bulletin_at: Some(now),
        };

        let output = format_local_status(&status);

        assert!(output.contains("publish: published · feed workstation"));
        assert!(output.contains("edge: 1 accepted · 1 feeds · 2 headlines"));
        assert!(output.contains("publish detail: edge accepted story capsule batch"));
    }

    #[test]
    fn auth_callback_page_uses_aberration_font_stack() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener binds");
        let addr = listener.local_addr().expect("listener addr");
        let client = TcpStream::connect(addr).expect("client connects");
        let (mut server, _) = listener.accept().expect("server accepts");

        write_auth_callback_response(&mut server, true, "github sign-in complete")
            .expect("callback response writes");
        drop(server);

        let mut response = String::new();
        client
            .take(8192)
            .read_to_string(&mut response)
            .expect("response reads");

        assert!(response.contains("font-family: ui-monospace, monospace;"));
        assert!(!response.contains("ui-sans-serif"));
        assert!(!response.contains("system-ui"));
    }

    #[test]
    fn workspace_filter_accepts_child_cwd_and_rejects_missing_cwd() {
        let root = PathBuf::from("/tmp/agent-feed-workspace");
        let filter = WorkspaceFilter::from_cli(Some(root.clone()))
            .expect("filter builds")
            .expect("filter enabled");

        let mut event = AgentEvent::new(
            agent_feed_core::SourceKind::Codex,
            agent_feed_core::EventKind::TurnComplete,
            "done",
        );
        event.cwd = Some(root.join("crate").display().to_string());
        assert!(filter.allows(&event));

        event.cwd = Some("/tmp/other-workspace".to_string());
        assert!(!filter.allows(&event));

        event.cwd = None;
        assert!(!filter.allows(&event));
    }

    #[test]
    fn codex_collection_filters_by_workspace_cwd() {
        let root = temp_test_root("codex-workspace-filter");
        let workspace = root.join("repo");
        let other = root.join("other");
        fs::create_dir_all(&workspace).expect("workspace dir");
        fs::create_dir_all(&other).expect("other dir");
        let transcript = root.join("codex.jsonl");
        write_jsonl(
            &transcript,
            [
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-04-24T03:16:49.696Z",
                    "payload": {"id": "codex-workspace-test", "cwd": workspace}
                }),
                json!({
                    "type": "turn_context",
                    "timestamp": "2026-04-24T03:16:49.697Z",
                    "payload": {"cwd": workspace, "turn_id": "turn_1"}
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:17:00.000Z",
                    "payload": {
                        "type": "exec_command_end",
                        "status": "completed",
                        "exit_code": 0,
                        "duration": "120ms",
                        "command": ["cargo", "test"]
                    }
                }),
                json!({
                    "type": "turn_context",
                    "timestamp": "2026-04-24T03:18:49.697Z",
                    "payload": {"cwd": other, "turn_id": "turn_2"}
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:19:00.000Z",
                    "payload": {
                        "type": "exec_command_end",
                        "status": "completed",
                        "exit_code": 0,
                        "duration": "120ms",
                        "command": ["cargo", "fmt"]
                    }
                }),
            ],
        );

        let filter = WorkspaceFilter::from_cli(Some(workspace.clone()))
            .expect("filter builds")
            .expect("filter enabled");
        let collected =
            collect_codex_events(&transcript, Some(&filter)).expect("codex collection works");
        assert!(collected.filtered > 0);
        assert!(!collected.events.is_empty());
        assert!(collected.events.iter().all(|event| filter.allows(event)));

        let unfiltered = collect_codex_events(&transcript, None).expect("unfiltered collection");
        assert!(unfiltered.events.len() > collected.events.len());

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn codex_transcript_compiles_meaningful_settled_stories() {
        let root = temp_test_root("codex-meaningful-stories");
        let workspace = root.join("repo");
        fs::create_dir_all(&workspace).expect("workspace dir");
        let transcript = root.join("codex.jsonl");
        write_jsonl(
            &transcript,
            [
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-04-24T03:16:49.696Z",
                    "payload": {"id": "codex-meaningful-test", "cwd": workspace}
                }),
                json!({
                    "type": "turn_context",
                    "timestamp": "2026-04-24T03:16:49.697Z",
                    "payload": {"cwd": workspace, "turn_id": "turn_1"}
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:17:00.000Z",
                    "payload": {
                        "type": "exec_command_end",
                        "status": "failed",
                        "exit_code": 1,
                        "duration": "120ms",
                        "command": ["/usr/bin/zsh", "-lc", "cargo test --all"]
                    }
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:17:02.000Z",
                    "payload": {
                        "type": "patch_apply_end",
                        "success": true,
                        "changes": {"src/lib.rs": {}, "crates/feed/src/main.rs": {}}
                    }
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:17:05.000Z",
                    "payload": {
                        "type": "task_complete",
                        "turn_id": "turn_1",
                        "last_agent_message": "Fixed the capture pipeline and left one integration test failing.",
                        "duration_ms": 15000
                    }
                }),
            ],
        );

        let stories = compile_codex_stories(std::slice::from_ref(&transcript), None)
            .expect("stories compile");
        let display = serde_json::to_string(&stories)
            .expect("stories serialize")
            .to_ascii_lowercase();

        assert!(
            stories
                .iter()
                .any(|story| story.family == StoryFamily::Test)
        );
        assert!(
            stories
                .iter()
                .any(|story| story.family == StoryFamily::Turn)
        );
        assert!(display.contains("failing tests") || display.contains("tests are red"));
        assert!(!display.contains("shell command failed"));
        assert!(!display.contains("cargo test --all"));

        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let summaries =
            summarize_feed("github:35904762:workstation", &stories, &config).expect("summarizes");
        let summary_display = serde_json::to_string(&summaries)
            .expect("summaries serialize")
            .to_ascii_lowercase();

        assert!(!summaries.is_empty());
        assert!(!summary_display.contains("shell command failed"));
        assert!(!summary_display.contains("cargo test --all"));

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn codex_startup_context_primes_future_completion_without_replaying_history() {
        let root = temp_test_root("codex-startup-context");
        let workspace = root.join("repo");
        fs::create_dir_all(&workspace).expect("workspace dir");
        let transcript = root.join("codex.jsonl");
        write_jsonl(
            &transcript,
            [
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-04-24T03:16:49.696Z",
                    "payload": {"id": "codex-startup-context", "cwd": workspace}
                }),
                json!({
                    "type": "turn_context",
                    "timestamp": "2026-04-24T03:16:49.697Z",
                    "payload": {"cwd": workspace, "turn_id": "turn_1"}
                }),
                json!({
                    "type": "response_item",
                    "timestamp": "2026-04-24T03:16:55.000Z",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "text",
                            "text": "Browser feed discovery now follows signed story streams so public viewers can see live publisher work."
                        }]
                    }
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:17:02.000Z",
                    "payload": {
                        "type": "patch_apply_end",
                        "success": true,
                        "changes": {"src/lib.rs": {}, "crates/feed/src/main.rs": {}}
                    }
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:17:04.000Z",
                    "payload": {
                        "type": "exec_command_end",
                        "status": "failed",
                        "exit_code": 1,
                        "duration": "120ms",
                        "command": ["cargo", "test", "--all"]
                    }
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:17:05.000Z",
                    "payload": {
                        "type": "task_complete",
                        "turn_id": "turn_1",
                        "duration_ms": 15000
                    }
                }),
            ],
        );

        let sample = transcript_suffix(&transcript, STARTUP_CONTEXT_TAIL_BYTES)
            .expect("startup context tail reads");
        let mut state = TranscriptState::default();
        warm_codex_state_from_sample(&transcript, &mut state).expect("state warms from sample");
        let mut compiler = StoryCompiler::default();
        let mut context_events = Vec::new();
        for line in sample
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            let value = serde_json::from_str::<Value>(line).expect("json line parses");
            let Some(event) = normalize_transcript_value(value, &mut state, Some(&transcript))
            else {
                continue;
            };
            if is_startup_context_event(&event) {
                let mut event = event;
                mark_startup_context_event(&mut event);
                context_events.push(event.clone());
                assert!(compiler.ingest(event).is_empty());
            }
        }

        assert!(
            context_events
                .iter()
                .any(|event| event.kind == agent_feed_core::EventKind::AgentMessage)
        );
        assert!(!context_events.iter().any(|event| matches!(
            event.kind,
            agent_feed_core::EventKind::TurnComplete
                | agent_feed_core::EventKind::FileChanged
                | agent_feed_core::EventKind::TestFail
        )));
        assert!(compiler.flush().is_empty());

        let future = json!({
            "type": "event_msg",
            "timestamp": "2026-04-24T03:18:05.000Z",
            "payload": {
                "type": "task_complete",
                "turn_id": "turn_1",
                "duration_ms": 30000
            }
        });
        let event = normalize_transcript_value(future, &mut state, Some(&transcript))
            .expect("future completion normalizes");
        let stories = compiler.ingest(event);

        let turn = stories
            .iter()
            .find(|story| story.family == StoryFamily::Turn)
            .expect("future completion publishes with startup context");
        assert!(turn.headline.contains("browser feed discovery"));
        assert!(!turn.headline.contains("turn completed"));
        assert!(!turn.deck.contains("2 changed files"));
        assert!(!turn.deck.contains("tests are red"));

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn codex_startup_recap_emits_recent_active_root_workspace_work() {
        let root = temp_test_root("codex-startup-recap-root");
        let workspace = root.join("repos");
        let burn_dragon = workspace.join("burn_dragon");
        fs::create_dir_all(&burn_dragon).expect("workspace dir");
        let transcript = root.join("codex.jsonl");
        write_jsonl(
            &transcript,
            [
                json!({
                    "type": "session_meta",
                    "payload": {"id": "root-active-session", "cwd": workspace}
                }),
                json!({
                    "type": "turn_context",
                    "payload": {"cwd": workspace, "turn_id": "turn_1"}
                }),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_1",
                        "arguments": {
                            "cmd": "gh run list --repo aberration-technology/burn_dragon --branch main",
                            "workdir": burn_dragon
                        }
                    }
                }),
            ],
        );
        let input = fs::read_to_string(&transcript).expect("transcript reads");

        let recap =
            codex_startup_recap_event(&transcript, &input, TranscriptState::default(), None, None)
                .expect("active root workspace recap emits");

        assert_eq!(recap.kind, EventKind::AgentMessage);
        assert_eq!(recap.project.as_deref(), Some("burn_dragon"));
        let burn_dragon_path = burn_dragon.to_string_lossy().to_string();
        assert_eq!(recap.cwd.as_deref(), Some(burn_dragon_path.as_str()));
        assert_eq!(
            recap.summary.as_deref(),
            Some("deployment workflow monitoring is active.")
        );
        assert!(recap.tags.iter().any(|tag| tag == "active-attach"));
        assert!(!recap.tags.iter().any(|tag| tag == STARTUP_CONTEXT_TAG));

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn codex_startup_recap_prefers_meaningful_agent_update_over_later_command() {
        let root = temp_test_root("codex-startup-recap-prefers-agent-update");
        let workspace = root.join("repos");
        let burn_dragon = workspace.join("burn_dragon");
        fs::create_dir_all(&burn_dragon).expect("workspace dir");
        let transcript = root.join("codex.jsonl");
        write_jsonl(
            &transcript,
            [
                json!({
                    "type": "session_meta",
                    "payload": {"id": "root-active-session", "cwd": workspace}
                }),
                json!({
                    "type": "turn_context",
                    "payload": {"cwd": workspace, "turn_id": "turn_1"}
                }),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "The workflow patch is deliberately narrow: clean target artifacts immediately before native-smoke and again before wasm-smoke. YAML parses cleanly, so burn_dragon CI can retry without changing runtime code."
                        }]
                    }
                }),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "call_id": "call_1",
                        "arguments": {
                            "cmd": "gh run watch 123 --repo aberration-technology/burn_dragon",
                            "workdir": burn_dragon
                        }
                    }
                }),
            ],
        );
        let input = fs::read_to_string(&transcript).expect("transcript reads");

        let recap =
            codex_startup_recap_event(&transcript, &input, TranscriptState::default(), None, None)
                .expect("active root workspace recap emits");

        assert_eq!(recap.project.as_deref(), Some("burn_dragon"));
        let summary = recap.summary.as_deref().expect("summary exists");
        assert!(summary.contains("workflow patch is deliberately narrow"));
        assert!(summary.contains("burn_dragon CI can retry"));
        assert_ne!(summary, "deployment workflow monitoring is active.");

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn codex_startup_recap_infers_project_from_root_assistant_context() {
        let root = temp_test_root("codex-startup-recap-project-context");
        let workspace = root.join("repos");
        fs::create_dir_all(&workspace).expect("workspace dir");
        let transcript = root.join("codex.jsonl");
        write_jsonl(
            &transcript,
            [
                json!({
                    "type": "session_meta",
                    "payload": {"id": "root-context-session", "cwd": workspace}
                }),
                json!({
                    "type": "turn_context",
                    "payload": {"cwd": workspace, "turn_id": "turn_1"}
                }),
                json!({
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "Burn_p2p browser peers now receive structured training receipts through the shared edge path."
                        }]
                    }
                }),
            ],
        );
        let input = fs::read_to_string(&transcript).expect("transcript reads");

        let recap =
            codex_startup_recap_event(&transcript, &input, TranscriptState::default(), None, None)
                .expect("assistant context recap emits");

        assert_eq!(recap.project.as_deref(), Some("burn_p2p"));
        assert_eq!(
            recap.summary.as_deref(),
            Some(
                "Burn_p2p browser peers now receive structured training receipts through the shared edge path."
            )
        );

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn claude_stream_compiles_meaningful_settled_stories() {
        let root = temp_test_root("claude-meaningful-stories");
        let workspace = root.join("repo");
        fs::create_dir_all(&workspace).expect("workspace dir");
        let stream = root.join("claude.jsonl");
        write_jsonl(
            &stream,
            [
                json!({
                    "type": "system",
                    "subtype": "init",
                    "session_id": "claude-meaningful-test",
                    "cwd": workspace,
                    "model": "claude-sonnet-4-6"
                }),
                json!({
                    "type": "assistant",
                    "message": {
                        "content": [{
                            "type": "tool_use",
                            "name": "Bash",
                            "input": {"command": "cargo test --all"}
                        }]
                    }
                }),
                json!({
                    "type": "tool_result",
                    "is_error": true,
                    "content": "raw failing output"
                }),
                json!({
                    "type": "result",
                    "subtype": "success",
                    "duration_ms": 9000,
                    "result": "Browser feed subscription verification failed before publish. The callback route needs a safer fix before public users rely on it."
                }),
            ],
        );

        let stories =
            compile_claude_stories(std::slice::from_ref(&stream), None).expect("stories compile");
        let display = serde_json::to_string(&stories)
            .expect("stories serialize")
            .to_ascii_lowercase();

        assert!(
            stories
                .iter()
                .any(|story| story.family == StoryFamily::Test)
        );
        assert!(
            stories
                .iter()
                .any(|story| story.family == StoryFamily::Turn)
        );
        assert!(display.contains("failing tests") || display.contains("tests are red"));
        assert!(!display.contains("shell command failed"));
        assert!(!display.contains("cargo test --all"));
        assert!(!display.contains("raw failing output"));

        let mut config = SummaryConfig::p2p_default();
        config.mode = FeedSummaryMode::PerStory;
        let summaries =
            summarize_feed("github:35904762:workstation", &stories, &config).expect("summarizes");
        let summary_display = serde_json::to_string(&summaries)
            .expect("summaries serialize")
            .to_ascii_lowercase();

        assert!(!summaries.is_empty());
        assert!(!summary_display.contains("shell command failed"));
        assert!(!summary_display.contains("cargo test --all"));
        assert!(!summary_display.contains("raw failing output"));

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn codex_workspace_scope_drops_events_without_cwd() {
        let root = temp_test_root("codex-missing-cwd");
        let workspace = root.join("repo");
        fs::create_dir_all(&workspace).expect("workspace dir");
        let transcript = root.join("codex.jsonl");
        write_jsonl(
            &transcript,
            [json!({
                "type": "event_msg",
                "timestamp": "2026-04-24T03:17:00.000Z",
                "payload": {"type": "task_started", "turn_id": "turn_1"}
            })],
        );

        let filter = WorkspaceFilter::from_cli(Some(workspace))
            .expect("filter builds")
            .expect("filter enabled");
        let collected =
            collect_codex_events(&transcript, Some(&filter)).expect("codex collection works");
        assert!(collected.events.is_empty());
        assert_eq!(collected.filtered, 1);

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn claude_collection_filters_by_workspace_cwd() {
        let root = temp_test_root("claude-workspace-filter");
        let workspace = root.join("repo");
        let other = root.join("other");
        fs::create_dir_all(&workspace).expect("workspace dir");
        fs::create_dir_all(&other).expect("other dir");
        let stream = root.join("claude.jsonl");
        write_jsonl(
            &stream,
            [
                json!({
                    "type": "system",
                    "subtype": "init",
                    "session_id": "claude-workspace-test",
                    "cwd": workspace,
                    "model": "claude-sonnet"
                }),
                json!({
                    "type": "assistant",
                    "message": {
                        "content": [{
                            "type": "tool_use",
                            "name": "Bash",
                            "input": {"command": "cargo test"}
                        }]
                    }
                }),
                json!({
                    "type": "system",
                    "subtype": "init",
                    "session_id": "claude-other-test",
                    "cwd": other,
                    "model": "claude-sonnet"
                }),
                json!({
                    "type": "result",
                    "subtype": "success",
                    "duration_ms": 1200,
                    "result": "raw answer omitted"
                }),
            ],
        );

        let filter = WorkspaceFilter::from_cli(Some(workspace.clone()))
            .expect("filter builds")
            .expect("filter enabled");
        let collected =
            collect_claude_events(&stream, Some(&filter)).expect("claude collection works");
        assert!(collected.filtered > 0);
        assert!(!collected.events.is_empty());
        assert!(collected.events.iter().all(|event| filter.allows(event)));

        let unfiltered = collect_claude_events(&stream, None).expect("unfiltered collection");
        assert!(unfiltered.events.len() > collected.events.len());

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn active_codex_workspace_scope_scans_past_latest_nonmatching_session() {
        let root = temp_test_root("active-codex-workspace-filter");
        let workspace = root.join("repo");
        let other = root.join("other");
        let sessions = root.join("sessions");
        fs::create_dir_all(&workspace).expect("workspace dir");
        fs::create_dir_all(&other).expect("other dir");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let history = root.join("history.jsonl");
        let inside = sessions.join("inside-session.jsonl");
        let outside = sessions.join("outside-session.jsonl");
        write_jsonl(
            &inside,
            [
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-04-24T03:16:49.696Z",
                    "payload": {"id": "inside-session", "cwd": workspace}
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:17:00.000Z",
                    "payload": {"type": "task_started", "turn_id": "turn-inside"}
                }),
            ],
        );
        write_jsonl(
            &outside,
            [
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-04-24T03:16:49.696Z",
                    "payload": {"id": "outside-session", "cwd": other}
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:17:00.000Z",
                    "payload": {"type": "task_started", "turn_id": "turn-outside"}
                }),
            ],
        );
        write_jsonl(
            &history,
            [
                json!({"session_id": "inside-session"}),
                json!({"session_id": "outside-session"}),
            ],
        );

        let filter = WorkspaceFilter::from_cli(Some(workspace))
            .expect("filter builds")
            .expect("filter enabled");
        let paths = active_codex_session_paths(&history, &sessions, 1, Some(&filter), None)
            .expect("active sessions resolve");

        assert_eq!(paths, vec![inside]);

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn active_codex_sessions_fall_back_to_session_mtime_without_history() {
        let root = temp_test_root("active-codex-mtime");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let first = sessions.join("first-session.jsonl");
        let second = sessions.join("second-session.jsonl");
        write_jsonl(
            &first,
            [json!({
                "type": "session_meta",
                "timestamp": "2026-04-24T03:16:49.696Z",
                "payload": {"id": "first-session", "cwd": root}
            })],
        );
        write_jsonl(
            &second,
            [json!({
                "type": "session_meta",
                "timestamp": "2026-04-24T03:16:50.696Z",
                "payload": {"id": "second-session", "cwd": root}
            })],
        );

        let paths = active_codex_session_paths(
            &root.join("missing-history.jsonl"),
            &sessions,
            2,
            None,
            None,
        )
        .expect("active sessions resolve from session files");

        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&first));
        assert!(paths.contains(&second));

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn active_codex_sessions_prioritize_process_backed_session_ids() {
        let root = temp_test_root("active-codex-process");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let active_id = "019dbbb4-b30f-7991-b0f6-1cbbcfebe0c2";
        let recent_id = "019dbd66-3c8a-7bb0-9abf-213204b8c6a8";
        let active = sessions.join(format!("rollout-2026-04-23T13-57-55-{active_id}.jsonl"));
        let recent = sessions.join(format!("rollout-2026-04-23T21-51-27-{recent_id}.jsonl"));
        write_jsonl(
            &active,
            [json!({
                "type": "session_meta",
                "timestamp": "2026-04-24T03:16:49.696Z",
                "payload": {"id": active_id, "cwd": root}
            })],
        );
        write_jsonl(
            &recent,
            [json!({
                "type": "session_meta",
                "timestamp": "2026-04-24T03:16:50.696Z",
                "payload": {"id": recent_id, "cwd": root}
            })],
        );

        let paths = active_codex_session_paths_with_process_ids(
            &root.join("missing-history.jsonl"),
            &sessions,
            1,
            None,
            None,
            &[active_id.to_string()],
        )
        .expect("active sessions resolve");

        assert_eq!(paths, vec![active]);
        assert_eq!(
            parse_codex_resume_session_id(&format!("/usr/local/bin/codex resume {active_id}")),
            Some(active_id.to_string())
        );

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn active_codex_sessions_skip_agent_feed_internal_summarizer_sessions() {
        let root = temp_test_root("active-codex-internal");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let internal = sessions.join("internal-summary.jsonl");
        let real = sessions.join("real-coding-session.jsonl");
        write_jsonl(
            &internal,
            [
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-04-24T03:16:49.696Z",
                    "payload": {"id": "internal-summary", "cwd": root}
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:16:50.000Z",
                    "payload": {
                        "type": "user_message",
                        "message": internal_story_summary_prompt()
                    }
                }),
            ],
        );
        write_jsonl(
            &real,
            [json!({
                "type": "session_meta",
                "timestamp": "2026-04-24T03:16:51.696Z",
                "payload": {"id": "real-coding-session", "cwd": root}
            })],
        );

        let paths = active_codex_session_paths(
            &root.join("missing-history.jsonl"),
            &sessions,
            4,
            None,
            None,
        )
        .expect("active sessions resolve");

        assert_eq!(paths, vec![real]);
        assert!(
            collect_codex_events(&internal, None)
                .expect("internal collection succeeds")
                .events
                .is_empty()
        );

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn active_codex_sessions_keep_root_workspace_session_that_mentions_internal_marker() {
        let root = temp_test_root("active-codex-root-real");
        let workspace = root.join("repos");
        let sessions = root.join("sessions");
        fs::create_dir_all(&workspace).expect("workspace dir");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let real = sessions.join("root-workspace-session.jsonl");
        write_jsonl(
            &real,
            [
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-04-24T03:16:49.696Z",
                    "payload": {"id": "root-workspace-session", "cwd": workspace}
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:16:50.000Z",
                    "payload": {
                        "type": "user_message",
                        "message": internal_story_summary_prompt()
                    }
                }),
                json!({
                    "type": "response_item",
                    "timestamp": "2026-04-24T03:17:00.000Z",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": "{\"cmd\":\"cargo test\",\"workdir\":\"/tmp/repos/burn_dragon\"}",
                        "call_id": "call_real"
                    }
                }),
            ],
        );

        let paths = active_codex_session_paths(
            &root.join("missing-history.jsonl"),
            &sessions,
            4,
            None,
            None,
        )
        .expect("active sessions resolve");

        assert_eq!(paths, vec![real.clone()]);
        assert!(
            !is_agent_feed_internal_transcript(
                &fs::read_to_string(&real).expect("real session reads")
            ),
            "real root workspace sessions should not be hidden by marker mentions"
        );

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn processor_guard_requires_owned_workdir_for_unregistered_internal_prompt() {
        let root = temp_test_root("processor-owned-workdir");
        let store = root.join("summary-memory.json");
        let work_dir = summary_memory_work_dir(&store);
        let workspace = root.join("repos");
        fs::create_dir_all(&work_dir).expect("processor work dir");
        fs::create_dir_all(&workspace).expect("workspace dir");
        let mut config = SummaryConfig::p2p_default();
        config.processor = SummaryProcessorConfig::CodexSessionMemory {
            store_path: store.display().to_string(),
            key: "feed:test".to_string(),
            command: "codex".to_string(),
        };
        let registry = ProcessorSessionRegistry::from_summary_config(&config);
        let processor_input = format!(
            "{}\n{}\n",
            json!({
                "type": "session_meta",
                "timestamp": "2026-04-24T03:16:49.696Z",
                "payload": {"id": "new-processor-session", "cwd": work_dir}
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-04-24T03:16:50.000Z",
                "payload": {
                    "type": "user_message",
                    "message": internal_story_summary_prompt()
                }
            })
        );
        let real_input = format!(
            "{}\n{}\n",
            json!({
                "type": "session_meta",
                "timestamp": "2026-04-24T03:16:49.696Z",
                "payload": {"id": "real-root-session", "cwd": workspace}
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-04-24T03:16:50.000Z",
                "payload": {
                    "type": "user_message",
                    "message": internal_story_summary_prompt()
                }
            })
        );

        assert!(is_agent_feed_internal_transcript_for_registry(
            &processor_input,
            None,
            Some(&registry)
        ));
        assert!(!is_agent_feed_internal_transcript_for_registry(
            &real_input,
            None,
            Some(&registry)
        ));

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn processor_guard_skips_default_memory_workdir_without_publish_registry() {
        let default_store = PathBuf::from(default_summary_memory_store_string());
        let default_work_dir = summary_memory_work_dir(&default_store);
        let input = format!(
            "{}\n",
            json!({
                "type": "session_meta",
                "payload": {"id": "default-processor-session", "cwd": default_work_dir}
            })
        );

        assert!(is_agent_feed_internal_transcript_for_registry(
            &input, None, None
        ));
        assert!(is_agent_feed_internal_transcript(&input));
    }

    #[test]
    fn transcript_startup_tail_respects_attach_boundary() {
        let root = temp_test_root("transcript-attach-boundary");
        let transcript = root.join("codex.jsonl");
        write_jsonl(
            &transcript,
            [json!({
                "type": "session_meta",
                "timestamp": "2026-04-24T03:16:49.696Z",
                "payload": {"id": "boundary-session", "cwd": root}
            })],
        );
        let attach_offset = fs::metadata(&transcript)
            .expect("transcript metadata")
            .len();
        {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .expect("transcript appends");
            writeln!(
                file,
                "{}",
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:17:00.000Z",
                    "payload": {"type": "exec_command_end", "turn_id": "future-turn"}
                })
            )
            .expect("future event writes");
        }

        let startup_tail =
            transcript_suffix_at(&transcript, STARTUP_CONTEXT_TAIL_BYTES, attach_offset)
                .expect("bounded startup tail reads");
        let full_tail = transcript_suffix(&transcript, STARTUP_CONTEXT_TAIL_BYTES)
            .expect("full startup tail reads");

        assert!(startup_tail.contains("boundary-session"));
        assert!(!startup_tail.contains("future-turn"));
        assert!(full_tail.contains("future-turn"));

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn processor_registry_skips_known_summary_session_without_prompt_scan() {
        let root = temp_test_root("active-codex-registry");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let store = root.join("summary-memory.json");
        fs::write(
            &store,
            json!({
                "records": {
                    "feed:test": {
                        "codex_session_id": "processor-session-1",
                        "memory_digest": "prior feed memory",
                        "semantic_fingerprint": "prior"
                    }
                }
            })
            .to_string(),
        )
        .expect("summary memory writes");
        let processor = sessions.join("processor-session-1.jsonl");
        let real = sessions.join("real-coding-session.jsonl");
        write_jsonl(
            &processor,
            [json!({
                "type": "session_meta",
                "timestamp": "2026-04-24T03:16:49.696Z",
                "payload": {"id": "processor-session-1", "cwd": root}
            })],
        );
        write_jsonl(
            &real,
            [json!({
                "type": "session_meta",
                "timestamp": "2026-04-24T03:16:51.696Z",
                "payload": {"id": "real-coding-session", "cwd": root}
            })],
        );

        let registry = ProcessorSessionRegistry::default();
        registry.add_summary_memory_path(store);
        registry.refresh();
        let paths = active_codex_session_paths(
            &root.join("missing-history.jsonl"),
            &sessions,
            4,
            None,
            Some(&registry),
        )
        .expect("active sessions resolve");
        let snapshot = registry.snapshot();

        assert_eq!(paths, vec![real]);
        assert_eq!(snapshot.known_sessions, 1);
        assert_eq!(snapshot.processor_sessions_skipped, 1);

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn processor_registry_drops_registered_events_before_publish() {
        let registry = ProcessorSessionRegistry::default();
        registry.register_session("processor-session-2");
        let mut event = AgentEvent::new(
            agent_feed_core::SourceKind::Codex,
            agent_feed_core::EventKind::AgentMessage,
            "internal summary output",
        );
        event.session_id = Some("processor-session-2".to_string());

        assert!(registry.drop_processor_event(&event));
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.known_sessions, 1);
        assert_eq!(snapshot.processor_events_dropped, 1);
    }

    #[test]
    fn active_claude_sessions_skip_agent_feed_internal_summarizer_sessions() {
        let root = temp_test_root("active-claude-internal");
        let projects = root.join("projects");
        fs::create_dir_all(&projects).expect("projects dir");
        let internal = projects.join("internal-summary.jsonl");
        let real = projects.join("real-coding-session.jsonl");
        write_jsonl(
            &internal,
            [json!({
                "type": "user",
                "message": {
                    "content": internal_story_summary_prompt()
                }
            })],
        );
        write_jsonl(
            &real,
            [json!({
                "type": "system",
                "session_id": "real-claude-session",
                "cwd": root
            })],
        );

        let paths =
            active_claude_session_paths(&projects, 4, None).expect("active sessions resolve");

        assert_eq!(paths, vec![real]);
        assert!(
            collect_claude_events(&internal, None)
                .expect("internal collection succeeds")
                .events
                .is_empty()
        );

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }

    #[test]
    fn summary_prompt_discussion_does_not_mark_real_session_internal() {
        let root = temp_test_root("active-codex-summary-discussion");
        let sessions = root.join("sessions");
        fs::create_dir_all(&sessions).expect("sessions dir");
        let real = sessions.join("real-summary-discussion.jsonl");
        write_jsonl(
            &real,
            [
                json!({
                    "type": "session_meta",
                    "timestamp": "2026-04-24T03:16:49.696Z",
                    "payload": {"id": "real-summary-discussion", "cwd": root}
                }),
                json!({
                    "type": "event_msg",
                    "timestamp": "2026-04-24T03:16:50.000Z",
                    "payload": {
                        "type": "user_message",
                        "message": "Please review this prompt: Return one JSON object with headline, deck, lower_third, chips. Use only the redacted story facts below."
                    }
                }),
            ],
        );

        let paths = active_codex_session_paths(
            &root.join("missing-history.jsonl"),
            &sessions,
            1,
            None,
            None,
        )
        .expect("active sessions resolve");

        assert_eq!(paths, vec![real]);

        fs::remove_dir_all(root).expect("cleanup temp workspace");
    }
}

fn endpoint_for_source(source: &str) -> &'static str {
    match source_from_str(source) {
        agent_feed_core::SourceKind::Codex => "/ingest/codex/jsonl",
        agent_feed_core::SourceKind::Claude => "/ingest/claude/stream-json",
        agent_feed_core::SourceKind::Mcp => "/ingest/mcp",
        agent_feed_core::SourceKind::Otel => "/ingest/otel",
        _ => "/ingest/generic",
    }
}

fn payloads_from_input(input: &str) -> Result<Vec<String>, CliError> {
    if let Ok(value) = serde_json::from_str::<Value>(input) {
        return match value {
            Value::Array(values) => values
                .into_iter()
                .map(|value| serde_json::to_string(&value).map_err(CliError::from))
                .collect(),
            other => Ok(vec![serde_json::to_string(&other)?]),
        };
    }

    input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let value = serde_json::from_str::<Value>(line)?;
            serde_json::to_string(&value).map_err(CliError::from)
        })
        .collect()
}

fn hook_payload(source: &str, event: &str, input: &str) -> String {
    if serde_json::from_str::<Value>(input).is_ok() {
        input.to_string()
    } else {
        json!({
            "source": source,
            "kind": "agent.message",
            "title": event,
            "summary": "hook event captured",
        })
        .to_string()
    }
}

fn import_codex_sessions(
    server: &str,
    paths: &[PathBuf],
    workspace: Option<&WorkspaceFilter>,
) -> Result<(), CliError> {
    if paths.is_empty() {
        println!("codex transcript import complete: 0 events; no sessions found");
        return Ok(());
    }

    let mut total = ImportStats::default();
    for path in paths {
        let input = fs::read_to_string(path)?;
        if is_agent_feed_internal_transcript(&input) {
            debug!(
                path = %path.display(),
                "codex transcript import skipped agent-feed internal processor session"
            );
            continue;
        }
        let mut state = TranscriptState::default();
        let file_stats =
            import_codex_chunk(server, path, &input, &mut state, workspace, None, None);
        total.add(file_stats);
        info!(
            path = %path.display(),
            events = file_stats.imported,
            filtered_events = file_stats.filtered,
            total_events = total.imported,
            total_filtered_events = total.filtered,
            workspace = workspace.map(WorkspaceFilter::display).unwrap_or_else(|| "all".to_string()),
            "codex transcript imported"
        );
        print_import_file("codex transcript", path, file_stats);
    }
    print_import_complete("codex transcript", total);
    Ok(())
}

fn compile_codex_stories(
    paths: &[PathBuf],
    workspace: Option<&WorkspaceFilter>,
) -> Result<Vec<CompiledStory>, CliError> {
    let mut events = Vec::new();
    let mut filtered = 0usize;
    for path in paths {
        let collected = collect_codex_events(path, workspace)?;
        filtered += collected.filtered;
        events.extend(collected.events);
    }
    info!(
        sessions = paths.len(),
        events = events.len(),
        filtered_events = filtered,
        workspace = workspace
            .map(WorkspaceFilter::display)
            .unwrap_or_else(|| "all".to_string()),
        "codex events collected for story compilation"
    );
    Ok(compile_events(events))
}

fn warm_codex_paths(paths: &[PathBuf]) {
    for path in paths {
        let Ok(input) = fs::read_to_string(path) else {
            continue;
        };
        let mut state = TranscriptState::default();
        warm_codex_state(path, &input, &mut state);
    }
}

fn import_claude_sessions(
    server: &str,
    paths: &[PathBuf],
    workspace: Option<&WorkspaceFilter>,
) -> Result<(), CliError> {
    if paths.is_empty() {
        println!("claude stream import complete: 0 events; no sessions found");
        return Ok(());
    }

    let mut total = ImportStats::default();
    for path in paths {
        let input = fs::read_to_string(path)?;
        let file_stats = import_claude_stream_input(server, path, &input, workspace);
        total.add(file_stats);
        info!(
            path = %path.display(),
            events = file_stats.imported,
            filtered_events = file_stats.filtered,
            total_events = total.imported,
            total_filtered_events = total.filtered,
            workspace = workspace.map(WorkspaceFilter::display).unwrap_or_else(|| "all".to_string()),
            "claude stream imported"
        );
        print_import_file("claude stream", path, file_stats);
    }
    print_import_complete("claude stream", total);
    Ok(())
}

fn compile_claude_stories(
    paths: &[PathBuf],
    workspace: Option<&WorkspaceFilter>,
) -> Result<Vec<CompiledStory>, CliError> {
    let mut events = Vec::new();
    let mut filtered = 0usize;
    for path in paths {
        let collected = collect_claude_events(path, workspace)?;
        filtered += collected.filtered;
        events.extend(collected.events);
    }
    info!(
        sessions = paths.len(),
        events = events.len(),
        filtered_events = filtered,
        workspace = workspace
            .map(WorkspaceFilter::display)
            .unwrap_or_else(|| "all".to_string()),
        "claude events collected for story compilation"
    );
    Ok(compile_events(events))
}

fn warm_claude_paths(paths: &[PathBuf]) {
    for path in paths {
        let Ok(input) = fs::read_to_string(path) else {
            continue;
        };
        let mut state = ClaudeState::default();
        warm_claude_state(path, &input, &mut state);
    }
}

fn collect_codex_events(
    path: &Path,
    workspace: Option<&WorkspaceFilter>,
) -> Result<CollectedEvents, CliError> {
    let input = fs::read_to_string(path)?;
    if is_agent_feed_internal_transcript(&input) {
        debug!(
            path = %path.display(),
            "codex transcript collection skipped agent-feed internal processor session"
        );
        return Ok(CollectedEvents::default());
    }
    let mut state = TranscriptState::default();
    let mut collected = CollectedEvents::default();
    for (index, line) in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    line = index + 1,
                    error = %err,
                    "codex transcript parse failed"
                );
                eprintln!(
                    "agent-feed: failed to parse codex transcript line {} in {}: {err}",
                    index + 1,
                    path.display()
                );
                continue;
            }
        };
        if let Some(event) = normalize_transcript_value(value, &mut state, Some(path)) {
            if event_matches_workspace(&event, workspace, path, "codex") {
                collected.events.push(event);
            } else {
                collected.filtered += 1;
            }
        }
    }
    Ok(collected)
}

fn collect_claude_events(
    path: &Path,
    workspace: Option<&WorkspaceFilter>,
) -> Result<CollectedEvents, CliError> {
    let input = fs::read_to_string(path)?;
    if is_agent_feed_internal_transcript(&input) {
        debug!(
            path = %path.display(),
            "claude stream collection skipped agent-feed internal processor session"
        );
        return Ok(CollectedEvents::default());
    }
    let mut state = ClaudeState::default();
    let mut collected = CollectedEvents::default();
    for (index, line) in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    line = index + 1,
                    error = %err,
                    "claude stream parse failed"
                );
                eprintln!(
                    "agent-feed: failed to parse claude stream line {} in {}: {err}",
                    index + 1,
                    path.display()
                );
                continue;
            }
        };
        if let Some(event) = normalize_stream_value(value, &mut state, Some(path)) {
            if event_matches_workspace(&event, workspace, path, "claude") {
                collected.events.push(event);
            } else {
                collected.filtered += 1;
            }
        }
    }
    Ok(collected)
}

fn print_import_file(label: &str, path: &Path, stats: ImportStats) {
    if stats.filtered == 0 {
        println!(
            "{label} imported: {} events from {}",
            stats.imported,
            path.display()
        );
    } else {
        println!(
            "{label} imported: {} events from {}; {} filtered outside workspace",
            stats.imported,
            path.display(),
            stats.filtered
        );
    }
}

fn print_watch_started(label: &str, path: &Path, stats: ImportStats) {
    if stats.filtered == 0 {
        println!(
            "{label} watching: {} ({} initial events)",
            path.display(),
            stats.imported
        );
    } else {
        println!(
            "{label} watching: {} ({} initial events; {} filtered outside workspace)",
            path.display(),
            stats.imported,
            stats.filtered
        );
    }
}

fn post_capture_health(server: &str, agent: &str, adapter: &str, path: &Path) {
    let session_id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .map(ToString::to_string);
    let body = match serde_json::to_string(&json!({
        "source": agent,
        "agent": agent,
        "adapter": adapter,
        "session_id": session_id,
        "kind": "adapter.health",
        "severity": "debug",
        "title": format!("{agent} capture active"),
        "summary": "watching future transcript events; history replay is disabled unless --include-history is set",
        "tags": ["capture", "future-only"],
        "score_hint": 5,
    })) {
        Ok(body) => body,
        Err(err) => {
            warn!(
                %agent,
                %adapter,
                path = %path.display(),
                error = %err,
                "capture health encode failed"
            );
            return;
        }
    };
    if let Err(err) = post_json(server, "/ingest/generic", &body) {
        warn!(
            %agent,
            %adapter,
            path = %path.display(),
            error = %err,
            "capture health post failed"
        );
    }
}

struct CaptureWatchPost<'a> {
    server: &'a str,
    agent: &'a str,
    adapter: &'a str,
    path: &'a Path,
    state: &'a str,
    stats: ImportStats,
    offset: u64,
    file_len: u64,
    poll_ms: u64,
    workspace: Option<&'a WorkspaceFilter>,
}

fn post_capture_watch_status(update: CaptureWatchPost<'_>) {
    let session_id = codex_session_id_from_path(update.path);
    let last_append_ms = fs::metadata(update.path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64);
    let body = match serde_json::to_string(&json!({
        "agent": update.agent,
        "adapter": update.adapter,
        "label": capture_watch_label(update.path),
        "state": update.state,
        "workspace": capture_status_workspace(update.workspace),
        "session_id": session_id,
        "last_append_ms": last_append_ms,
        "offset": update.offset,
        "file_len": update.file_len,
        "imported_events": update.stats.imported,
        "filtered_events": update.stats.filtered,
        "poll_ms": update.poll_ms,
    })) {
        Ok(body) => body,
        Err(err) => {
            warn!(
                agent = %update.agent,
                adapter = %update.adapter,
                path = %update.path.display(),
                error = %err,
                "capture watcher status encode failed"
            );
            return;
        }
    };
    if let Err(err) = post_json(update.server, "/capture/status", &body) {
        warn!(
            agent = %update.agent,
            adapter = %update.adapter,
            path = %update.path.display(),
            state = %update.state,
            error = %err,
            "capture watcher status post failed"
        );
    }
}

fn capture_watch_label(path: &Path) -> String {
    let label = path
        .file_name()
        .or_else(|| path.file_stem())
        .and_then(|value| value.to_str())
        .unwrap_or("session")
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(*ch, '-' | '_' | '.'))
        .take(96)
        .collect::<String>();
    if label.is_empty() {
        "session".to_string()
    } else {
        label
    }
}

fn capture_status_workspace(workspace: Option<&WorkspaceFilter>) -> Option<&'static str> {
    Some(if workspace.is_some() {
        "workspace"
    } else {
        "all"
    })
}

fn print_watch_appended(label: &str, path: &Path, stats: ImportStats) {
    if stats.filtered == 0 {
        println!(
            "{label} imported: {} appended events from {}",
            stats.imported,
            path.display()
        );
    } else {
        println!(
            "{label} imported: {} appended events from {}; {} filtered outside workspace",
            stats.imported,
            path.display(),
            stats.filtered
        );
    }
}

fn print_stories(
    agent: &str,
    paths: &[PathBuf],
    stories: &[CompiledStory],
) -> Result<(), CliError> {
    println!(
        "{agent} stories compiled: {} stories from {} sessions",
        stories.len(),
        paths.len()
    );
    info!(
        %agent,
        stories = stories.len(),
        sessions = paths.len(),
        "stories compiled"
    );
    for story in stories {
        println!("{}", serde_json::to_string(story)?);
    }
    Ok(())
}

fn signed_capsules(
    feed: &str,
    stories: &[CompiledStory],
    summary_config: &SummaryConfig,
    publisher: Option<&PublisherIdentity>,
    signing_secret: Option<&str>,
) -> Result<Vec<Signed<StoryCapsule>>, CliError> {
    let feed_id = publish_feed_id_for_publisher(feed, publisher);
    let summaries = summarize_feed(&feed_id, stories, summary_config)?;
    info!(
        %feed_id,
        stories = stories.len(),
        summaries = summaries.len(),
        "feed stories summarized"
    );
    signed_capsules_from_summaries(feed, &summaries, publisher, signing_secret, 1)
}

fn signed_capsules_from_summaries(
    feed: &str,
    summaries: &[FeedSummary],
    publisher: Option<&PublisherIdentity>,
    signing_secret: Option<&str>,
    start_seq: u64,
) -> Result<Vec<Signed<StoryCapsule>>, CliError> {
    let feed_id = publish_feed_id_for_publisher(feed, publisher);
    signed_capsules_from_summaries_with_feed_id(
        &feed_id,
        summaries,
        publisher,
        signing_secret,
        start_seq,
    )
}

fn signed_capsules_from_summaries_with_feed_id(
    feed_id: &str,
    summaries: &[FeedSummary],
    publisher: Option<&PublisherIdentity>,
    signing_secret: Option<&str>,
    start_seq: u64,
) -> Result<Vec<Signed<StoryCapsule>>, CliError> {
    summaries
        .iter()
        .enumerate()
        .map(|(index, summary)| {
            let mut capsule = StoryCapsule::from_summary(
                feed_id.to_string(),
                start_seq + index as u64,
                "local:codex",
                summary,
            )?;
            if let Some(publisher) = publisher {
                capsule = capsule.with_publisher(publisher.clone())?;
            }
            if let Some(secret) = signing_secret {
                let key_id = publisher
                    .and_then(|publisher| publisher.github_user_id)
                    .map(|id| format!("github:{id}"))
                    .unwrap_or_else(|| "local-codex".to_string());
                Signed::sign_capsule_with_secret(capsule, &key_id, secret).map_err(CliError::from)
            } else {
                Signed::sign_capsule(capsule, "local-codex").map_err(CliError::from)
            }
        })
        .collect()
}

fn publish_feed_id_for_publisher(feed: &str, publisher: Option<&PublisherIdentity>) -> String {
    match publisher.and_then(|publisher| publisher.github_user_id) {
        Some(github_user_id) => format!("github:{github_user_id}:{feed}"),
        None => local_feed_id(feed),
    }
}

struct SummaryCliOptions<'a> {
    summarizer: &'a str,
    summary_style: &'a str,
    summary_prompt_max_chars: usize,
    per_story: bool,
    allow_project_names: bool,
    summary_memory_store: Option<&'a Path>,
    summary_endpoint: Option<&'a str>,
    summary_auth_header_env: Option<&'a str>,
    summary_command: Option<&'a str>,
    summary_args: &'a [String],
    guardrail_patterns: &'a [String],
    images: bool,
    image_processor: &'a str,
    image_endpoint: Option<&'a str>,
    image_command: Option<&'a str>,
    image_args: &'a [String],
    image_style: Option<&'a str>,
    image_prompt_max_chars: Option<usize>,
    allow_remote_image_urls: bool,
}

fn summary_config(options: SummaryCliOptions<'_>) -> Result<SummaryConfig, CliError> {
    let mut config = SummaryConfig::p2p_default();
    config.mode = if options.per_story {
        FeedSummaryMode::PerStory
    } else {
        FeedSummaryMode::FeedRollup
    };
    config.prompt.style = options.summary_style.to_string();
    config.prompt.max_prompt_chars = options.summary_prompt_max_chars.max(512);
    config.processor = match options.summarizer {
        "deterministic" | "offline" => SummaryProcessorConfig::Deterministic,
        "aesthetic" | "codex" | "codex-exec" => SummaryProcessorConfig::CodexExec,
        "codex-memory" | "codex-session" | "aesthetic-memory" => {
            SummaryProcessorConfig::CodexSessionMemory {
                store_path: options
                    .summary_memory_store
                    .map(Path::display)
                    .map(|path| path.to_string())
                    .unwrap_or_else(default_summary_memory_store_string),
                key: "default".to_string(),
                command: default_codex_command(),
            }
        }
        "claude" | "claude-code" | "claude-code-exec" => SummaryProcessorConfig::ClaudeCodeExec,
        "process" | "command" => SummaryProcessorConfig::Process {
            command: options
                .summary_command
                .ok_or_else(|| {
                    CliError::Http(
                        "--summary-command is required for process summarizer".to_string(),
                    )
                })?
                .to_string(),
            args: options.summary_args.to_vec(),
        },
        "http" | "http-endpoint" => SummaryProcessorConfig::HttpEndpoint {
            url: options
                .summary_endpoint
                .ok_or_else(|| {
                    CliError::Http("--summary-endpoint is required for http summarizer".to_string())
                })?
                .to_string(),
            auth_header_env: options.summary_auth_header_env.map(str::to_string),
        },
        other => {
            return Err(CliError::Http(format!(
                "unknown summarizer {other}; use codex-memory, aesthetic, codex-exec, claude-code, process, http-endpoint, or deterministic"
            )));
        }
    };
    config.guardrails.allow_project_names = options.allow_project_names;
    config.image.enabled = options.images;
    config.image.allow_remote_urls = options.allow_remote_image_urls;
    config.image.decision = ImageDecisionMode::BestJudgement;
    if let Some(style) = options.image_style {
        config.image.prompt_style = style.to_string();
    }
    if let Some(max_prompt_chars) = options.image_prompt_max_chars {
        config.image.max_prompt_chars = max_prompt_chars.max(512);
    }
    config.image.processor = if options.images {
        parse_image_processor(
            options.image_processor,
            options.image_endpoint,
            options.image_command,
            options.image_args,
        )?
    } else {
        ImageProcessorConfig::Disabled
    };
    for (index, pattern) in options.guardrail_patterns.iter().enumerate() {
        config.guardrails.patterns.push(GuardrailPattern::reject(
            format!("cli-guardrail-{index}"),
            pattern.clone(),
        ));
    }
    Ok(config)
}

fn scope_summary_memory(
    config: &mut SummaryConfig,
    feed: &str,
    selected_agents: &HashSet<&str>,
    workspace: Option<&WorkspaceFilter>,
    reset: bool,
) -> Result<(), CliError> {
    let SummaryProcessorConfig::CodexSessionMemory {
        store_path, key, ..
    } = &mut config.processor
    else {
        return Ok(());
    };
    let agents = if selected_agents.is_empty() {
        "agents".to_string()
    } else {
        let mut agents = selected_agents.iter().copied().collect::<Vec<_>>();
        agents.sort();
        agents.join("+")
    };
    let workspace_scope = workspace
        .map(WorkspaceFilter::display)
        .unwrap_or_else(|| "all-workspaces".to_string());
    *key = format!("feed:{feed}:agents:{agents}:workspace:{workspace_scope}");
    if reset {
        let path = PathBuf::from(store_path.clone());
        if path.exists() {
            fs::remove_file(&path)?;
            info!(path = %path.display(), "summary memory store reset");
        }
    }
    Ok(())
}

fn default_summary_memory_store_string() -> String {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agent_feed")
        .join("summary-memory.json")
        .display()
        .to_string()
}

fn default_codex_command() -> String {
    std::env::var("AGENT_FEED_CODEX_BIN").unwrap_or_else(|_| "codex".to_string())
}

fn parse_image_processor(
    processor: &str,
    endpoint: Option<&str>,
    command: Option<&str>,
    args: &[String],
) -> Result<ImageProcessorConfig, CliError> {
    match processor {
        "codex" | "codex-exec" => Ok(ImageProcessorConfig::CodexExec),
        "claude" | "claude-code" | "claude-code-exec" => Ok(ImageProcessorConfig::ClaudeCodeExec),
        "process" | "command" => Ok(ImageProcessorConfig::Process {
            command: command
                .ok_or_else(|| {
                    CliError::Http("--image-command is required for process image processor".into())
                })?
                .to_string(),
            args: args.to_vec(),
        }),
        "http" | "http-endpoint" => Ok(ImageProcessorConfig::HttpEndpoint {
            url: endpoint
                .ok_or_else(|| {
                    CliError::Http("--image-endpoint is required for http image processor".into())
                })?
                .to_string(),
            auth_header_env: None,
        }),
        "disabled" | "none" => Ok(ImageProcessorConfig::Disabled),
        other => Err(CliError::Http(format!(
            "unknown image processor {other}; use codex-exec, claude-code, process, http-endpoint, or disabled"
        ))),
    }
}

fn parse_visibility(value: &str) -> FeedVisibility {
    match value {
        "public" => FeedVisibility::Public,
        "github_user" => FeedVisibility::GithubUser,
        "github_org" => FeedVisibility::GithubOrg,
        "github_team" => FeedVisibility::GithubTeam,
        "github_repo" => FeedVisibility::GithubRepo,
        _ => FeedVisibility::Private,
    }
}

struct CodexActiveWatch<'a> {
    server: &'a str,
    history: &'a Path,
    sessions_dir: &'a Path,
    sessions: usize,
    poll_ms: u64,
    workspace: Option<&'a WorkspaceFilter>,
    include_history: bool,
    event_sink: Option<&'a mpsc::Sender<AgentEvent>>,
    processor_registry: ProcessorSessionRegistry,
}

fn watch_codex_active_sessions(config: CodexActiveWatch<'_>) -> Result<(), CliError> {
    let mut watchers = Vec::new();
    let mut seen = HashSet::<PathBuf>::new();
    let mut reported_empty = false;
    let rediscover_every = Duration::from_secs(5);
    let mut last_discovery = Instant::now() - rediscover_every;

    loop {
        if last_discovery.elapsed() >= rediscover_every {
            let paths = active_codex_session_paths(
                config.history,
                config.sessions_dir,
                config.sessions,
                config.workspace,
                Some(&config.processor_registry),
            )?;
            for path in paths {
                if !seen.insert(path.clone()) {
                    continue;
                }
                let (state, stats, offset) = initialize_codex_watcher(
                    config.server,
                    &path,
                    config.workspace,
                    config.include_history,
                    config.event_sink,
                    Some(&config.processor_registry),
                )?;
                info!(
                    path = %path.display(),
                    initial_events = stats.imported,
                    filtered_events = stats.filtered,
                    offset,
                    poll_ms = config.poll_ms,
                    workspace = config.workspace.map(WorkspaceFilter::display).unwrap_or_else(|| "all".to_string()),
                    "codex transcript watcher discovered"
                );
                print_watch_started("codex transcript", &path, stats);
                post_capture_health(config.server, "codex", "codex.transcript", &path);
                post_capture_watch_status(CaptureWatchPost {
                    server: config.server,
                    agent: "codex",
                    adapter: "codex.transcript",
                    path: &path,
                    state: "watching",
                    stats,
                    offset,
                    file_len: offset,
                    poll_ms: config.poll_ms,
                    workspace: config.workspace,
                });
                watchers.push(CodexWatcher {
                    path,
                    offset,
                    state,
                    pending: String::new(),
                    last_status_at: Instant::now(),
                    internal_processor: false,
                    total_stats: stats,
                });
            }

            if watchers.is_empty() && !reported_empty {
                println!(
                    "codex capture: no active sessions found for {}; still watching for new sessions",
                    capture_scope_message(config.workspace)
                );
                reported_empty = true;
            } else if !watchers.is_empty() {
                reported_empty = false;
            }
            last_discovery = Instant::now();
        }

        poll_codex_watchers(
            config.server,
            &mut watchers,
            config.poll_ms,
            config.workspace,
            config.event_sink,
            Some(&config.processor_registry),
        )?;
        std::thread::sleep(Duration::from_millis(config.poll_ms.max(100)));
    }
}

fn watch_codex_sessions(
    server: &str,
    paths: &[PathBuf],
    poll_ms: u64,
    workspace: Option<&WorkspaceFilter>,
    include_history: bool,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
    processor_registry: Option<&ProcessorSessionRegistry>,
) -> Result<(), CliError> {
    let mut watchers = Vec::new();
    for path in paths {
        let (state, stats, offset) = initialize_codex_watcher(
            server,
            path,
            workspace,
            include_history,
            event_sink,
            processor_registry,
        )?;
        info!(
            path = %path.display(),
            initial_events = stats.imported,
            filtered_events = stats.filtered,
            offset,
            poll_ms,
            workspace = workspace.map(WorkspaceFilter::display).unwrap_or_else(|| "all".to_string()),
            "codex transcript watcher started"
        );
        print_watch_started("codex transcript", path, stats);
        post_capture_health(server, "codex", "codex.transcript", path);
        post_capture_watch_status(CaptureWatchPost {
            server,
            agent: "codex",
            adapter: "codex.transcript",
            path,
            state: "watching",
            stats,
            offset,
            file_len: offset,
            poll_ms,
            workspace,
        });
        watchers.push(CodexWatcher {
            path: path.clone(),
            offset,
            state,
            pending: String::new(),
            last_status_at: Instant::now(),
            internal_processor: false,
            total_stats: stats,
        });
    }

    loop {
        std::thread::sleep(Duration::from_millis(poll_ms.max(100)));
        poll_codex_watchers(
            server,
            &mut watchers,
            poll_ms,
            workspace,
            event_sink,
            processor_registry,
        )?;
    }
}

fn poll_codex_watchers(
    server: &str,
    watchers: &mut [CodexWatcher],
    poll_ms: u64,
    workspace: Option<&WorkspaceFilter>,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
    processor_registry: Option<&ProcessorSessionRegistry>,
) -> Result<(), CliError> {
    for watcher in watchers {
        let len = fs::metadata(&watcher.path)?.len();
        if len < watcher.offset {
            warn!(
                path = %watcher.path.display(),
                previous_offset = watcher.offset,
                len,
                "codex transcript shrank; resetting watcher offset"
            );
            watcher.offset = 0;
            watcher.pending.clear();
            watcher.internal_processor = false;
            watcher.total_stats = ImportStats::default();
            post_capture_watch_status(CaptureWatchPost {
                server,
                agent: "codex",
                adapter: "codex.transcript",
                path: &watcher.path,
                state: "truncated",
                stats: ImportStats::default(),
                offset: watcher.offset,
                file_len: len,
                poll_ms,
                workspace,
            });
            watcher.last_status_at = Instant::now();
        }
        if len <= watcher.offset {
            if watcher.last_status_at.elapsed() >= CAPTURE_STATUS_HEARTBEAT {
                post_capture_watch_status(CaptureWatchPost {
                    server,
                    agent: "codex",
                    adapter: "codex.transcript",
                    path: &watcher.path,
                    state: "watching",
                    stats: watcher.total_stats,
                    offset: watcher.offset,
                    file_len: len,
                    poll_ms,
                    workspace,
                });
                watcher.last_status_at = Instant::now();
            }
            continue;
        }
        let mut file = File::open(&watcher.path)?;
        file.seek(SeekFrom::Start(watcher.offset))?;
        let mut chunk = String::new();
        file.read_to_string(&mut chunk)?;
        watcher.offset = len;
        watcher.pending.push_str(&chunk);
        let complete = split_complete_jsonl(&mut watcher.pending);
        let processor_chunk = is_agent_feed_internal_transcript_for_registry(
            &complete,
            Some(&watcher.state),
            processor_registry,
        );
        if watcher.internal_processor || processor_chunk {
            if processor_chunk
                && confirmed_agent_feed_internal_processor_chunk(
                    &complete,
                    &watcher.state,
                    processor_registry,
                )
            {
                watcher.internal_processor = true;
            }
            let session_id =
                codex_session_id_from_input(&complete).or_else(|| watcher.state.session_id.clone());
            if let Some(registry) = processor_registry {
                registry.record_processor_session_skipped(session_id.as_deref());
            }
            debug!(
                path = %watcher.path.display(),
                session_id = session_id.as_deref().unwrap_or("<unknown>"),
                "codex transcript appended chunk skipped processor-owned session"
            );
            post_capture_watch_status(CaptureWatchPost {
                server,
                agent: "codex",
                adapter: "codex.transcript",
                path: &watcher.path,
                state: "processor-skipped",
                stats: watcher.total_stats,
                offset: watcher.offset,
                file_len: len,
                poll_ms,
                workspace,
            });
            watcher.last_status_at = Instant::now();
            continue;
        }
        let stats = import_codex_chunk(
            server,
            &watcher.path,
            &complete,
            &mut watcher.state,
            workspace,
            event_sink,
            processor_registry,
        );
        if stats.imported > 0 || stats.filtered > 0 {
            watcher.total_stats.add(stats);
            info!(
                path = %watcher.path.display(),
                events = stats.imported,
                filtered_events = stats.filtered,
                offset = watcher.offset,
                poll_ms,
                "codex transcript appended events imported"
            );
            print_watch_appended("codex transcript", &watcher.path, stats);
        }
        post_capture_watch_status(CaptureWatchPost {
            server,
            agent: "codex",
            adapter: "codex.transcript",
            path: &watcher.path,
            state: "appended",
            stats: watcher.total_stats,
            offset: watcher.offset,
            file_len: len,
            poll_ms,
            workspace,
        });
        watcher.last_status_at = Instant::now();
    }
    Ok(())
}

fn import_codex_chunk(
    server: &str,
    path: &Path,
    input: &str,
    state: &mut TranscriptState,
    workspace: Option<&WorkspaceFilter>,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
    processor_registry: Option<&ProcessorSessionRegistry>,
) -> ImportStats {
    let mut stats = ImportStats::default();
    if is_agent_feed_internal_transcript_for_registry(input, Some(state), processor_registry) {
        debug!(
            path = %path.display(),
            "codex transcript chunk skipped agent-feed internal processor session"
        );
        if let Some(registry) = processor_registry {
            let session_id =
                codex_session_id_from_input(input).or_else(|| state.session_id.clone());
            registry.record_processor_session_skipped(session_id.as_deref());
        }
        return stats;
    }
    for (index, line) in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    line = index + 1,
                    error = %err,
                    "codex transcript parse failed during import"
                );
                eprintln!(
                    "agent-feed: failed to parse codex transcript line {} in {}: {err}",
                    index + 1,
                    path.display()
                );
                continue;
            }
        };

        let Some(event) = normalize_transcript_value(value, state, Some(path)) else {
            continue;
        };
        if processor_registry.is_some_and(|registry| registry.drop_processor_event(&event)) {
            stats.filtered += 1;
            continue;
        }
        if !event_matches_workspace(&event, workspace, path, "codex") {
            stats.filtered += 1;
            continue;
        }
        let body = match serde_json::to_string(&event) {
            Ok(body) => body,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "codex event encode failed"
                );
                eprintln!(
                    "agent-feed: failed to encode codex event from {}: {err}",
                    path.display()
                );
                continue;
            }
        };
        if let Err(err) = post_json(server, "/ingest/codex/jsonl", &body) {
            warn!(
                path = %path.display(),
                %server,
                error = %err,
                "codex event post failed"
            );
            eprintln!(
                "agent-feed: failed to post codex event from {}: {err}",
                path.display()
            );
            continue;
        }
        debug!(
            path = %path.display(),
            index,
            event_id = %event.id,
            kind = ?event.kind,
            severity = ?event.severity,
            "codex event outgoing"
        );
        if let Some(sender) = event_sink
            && let Err(err) = sender.send(event.clone())
        {
            warn!(
                path = %path.display(),
                event_id = %event.id,
                error = %err,
                "codex event publish queue send failed"
            );
        }
        stats.imported += 1;
    }
    stats
}

fn initialize_codex_watcher(
    server: &str,
    path: &Path,
    workspace: Option<&WorkspaceFilter>,
    include_history: bool,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
    processor_registry: Option<&ProcessorSessionRegistry>,
) -> Result<(TranscriptState, ImportStats, u64), CliError> {
    let initial_len = fs::metadata(path)?.len();
    let mut state = TranscriptState::default();
    let stats = if include_history {
        let input = transcript_prefix(path, initial_len)?;
        import_codex_chunk(
            server,
            path,
            &input,
            &mut state,
            workspace,
            event_sink,
            processor_registry,
        )
    } else {
        warm_codex_state_from_sample_at(path, initial_len, &mut state)?;
        let context = transcript_suffix_at(path, STARTUP_CONTEXT_TAIL_BYTES, initial_len)?;
        let recap_state = state.clone();
        let mut stats = import_codex_context_chunk(
            server,
            path,
            &context,
            &mut state,
            workspace,
            event_sink,
            processor_registry,
        );
        if let Some(recap) =
            codex_startup_recap_event(path, &context, recap_state, workspace, processor_registry)
            && post_codex_synthetic_event(server, path, &recap, event_sink, "startup recap")
        {
            stats.imported += 1;
        }
        stats
    };
    Ok((state, stats, initial_len))
}

fn post_codex_synthetic_event(
    server: &str,
    path: &Path,
    event: &AgentEvent,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
    label: &'static str,
) -> bool {
    let body = match serde_json::to_string(event) {
        Ok(body) => body,
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                %label,
                "codex synthetic event encode failed"
            );
            return false;
        }
    };
    if let Err(err) = post_json(server, "/ingest/codex/jsonl", &body) {
        warn!(
            path = %path.display(),
            %server,
            error = %err,
            %label,
            "codex synthetic event post failed"
        );
        return false;
    }
    if let Some(sender) = event_sink
        && let Err(err) = sender.send(event.clone())
    {
        warn!(
            path = %path.display(),
            event_id = %event.id,
            error = %err,
            %label,
            "codex synthetic event publish queue send failed"
        );
    }
    info!(
        path = %path.display(),
        event_id = %event.id,
        project = event.project.as_deref().unwrap_or("local"),
        summary = event.summary.as_deref().unwrap_or_default(),
        %label,
        "codex active session startup recap emitted"
    );
    true
}

fn codex_startup_recap_event(
    path: &Path,
    input: &str,
    mut state: TranscriptState,
    workspace: Option<&WorkspaceFilter>,
    processor_registry: Option<&ProcessorSessionRegistry>,
) -> Option<AgentEvent> {
    if is_agent_feed_internal_transcript_for_registry(input, Some(&state), processor_registry) {
        return None;
    }

    let mut candidate = None::<(AgentEvent, String, u8, time::OffsetDateTime)>;
    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(event) = normalize_transcript_value(value, &mut state, Some(path)) else {
            continue;
        };
        if processor_registry.is_some_and(|registry| registry.drop_processor_event(&event)) {
            continue;
        }
        if !event_matches_workspace(&event, workspace, path, "codex") {
            continue;
        }
        if !event_is_recent_for_attach(&event) {
            continue;
        }
        let Some(summary) = attach_recap_summary(&event) else {
            continue;
        };
        let rank = attach_recap_rank(&event, &summary);
        let occurred_at = event.occurred_at.unwrap_or(event.received_at);
        let should_replace = candidate
            .as_ref()
            .is_none_or(|(_, _, existing_rank, existing_at)| {
                rank > *existing_rank || (rank == *existing_rank && occurred_at > *existing_at)
            });
        if should_replace {
            candidate = Some((event, summary, rank, occurred_at));
        }
    }

    let (event, summary, _, _) = candidate?;
    let mut recap = AgentEvent::new(
        SourceKind::Codex,
        EventKind::AgentMessage,
        "codex active session recap",
    );
    recap.agent = "codex".to_string();
    recap.adapter = "codex.transcript".to_string();
    recap.session_id = event.session_id;
    recap.turn_id = event.turn_id;
    recap.project = event.project;
    recap.cwd = event.cwd;
    recap.occurred_at = event.occurred_at;
    recap.summary = Some(summary);
    recap.tags = vec![
        "codex".to_string(),
        "transcript".to_string(),
        "active-attach".to_string(),
    ];
    recap.score_hint = Some(74);
    recap.severity = Severity::Notice;
    Some(recap)
}

fn attach_recap_rank(event: &AgentEvent, summary: &str) -> u8 {
    match event.kind {
        EventKind::AgentMessage | EventKind::TurnComplete | EventKind::TurnFail
            if attach_summary_has_context(summary) =>
        {
            100
        }
        EventKind::TestFail | EventKind::PermissionDenied | EventKind::McpFail => 90,
        EventKind::ToolComplete | EventKind::ToolFail if attach_summary_has_context(summary) => 80,
        EventKind::CommandExec | EventKind::ToolStart | EventKind::ToolComplete => 40,
        _ => 10,
    }
}

fn event_is_recent_for_attach(event: &AgentEvent) -> bool {
    let occurred = event.occurred_at.unwrap_or(event.received_at);
    let age = time::OffsetDateTime::now_utc() - occurred;
    age >= -time::Duration::minutes(5) && age <= STARTUP_RECAP_MAX_AGE
}

fn attach_recap_summary(event: &AgentEvent) -> Option<String> {
    if matches!(
        event.kind,
        EventKind::AgentMessage
            | EventKind::TurnComplete
            | EventKind::TurnFail
            | EventKind::TestFail
            | EventKind::PermissionDenied
            | EventKind::McpFail
    ) && let Some(summary) = event
        .summary
        .as_deref()
        .filter(|summary| attach_summary_has_context(summary))
    {
        return Some(summary.trim().to_string());
    }
    active_command_recap_summary(event)
}

fn attach_summary_has_context(summary: &str) -> bool {
    let lowered = summary.to_ascii_lowercase();
    if summary.len() < 24
        || lowered.contains("raw output omitted")
        || lowered.contains("raw diff omitted")
        || lowered.contains("command lifecycle captured")
        || lowered.starts_with("turn completed in ")
        || lowered.starts_with("model ")
    {
        return false;
    }
    [
        "auth",
        "aws",
        "browser",
        "burn_",
        "callback",
        "canary",
        "deployment",
        "diloco",
        "edge",
        "github",
        "gpu",
        "network",
        "p2p",
        "pages",
        "publish",
        "release",
        "terraform",
        "training",
        "webgpu",
        "workflow",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn active_command_recap_summary(event: &AgentEvent) -> Option<String> {
    if !matches!(
        event.kind,
        EventKind::CommandExec | EventKind::ToolStart | EventKind::ToolComplete
    ) {
        return None;
    }
    let command = event.command.as_deref()?.to_ascii_lowercase();
    if command.contains("receipt") && command.contains("training") {
        return Some("browser training receipt verification is active.".to_string());
    }
    if command.contains("burn_p2p_browser") || command.contains("browser") {
        return Some("browser peer verification is active.".to_string());
    }
    if command.contains("gh run") || command.contains("workflow") {
        return Some("deployment workflow monitoring is active.".to_string());
    }
    if command.contains("terraform") {
        return Some("infrastructure deployment work is active.".to_string());
    }
    if command.contains("cargo test")
        || command.contains("nextest")
        || command.contains("pytest")
        || command.contains("npm test")
    {
        if event.project.as_deref() == Some("burn_p2p") {
            return Some("p2p verification is active.".to_string());
        }
        if event.project.as_deref() == Some("burn_dragon") {
            return Some("browser training verification is active.".to_string());
        }
        return None;
    }
    if command.contains("training") || command.contains("webgpu") || command.contains("diloco") {
        return Some("training system work is active.".to_string());
    }
    None
}

fn import_codex_context_chunk(
    server: &str,
    path: &Path,
    input: &str,
    state: &mut TranscriptState,
    workspace: Option<&WorkspaceFilter>,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
    processor_registry: Option<&ProcessorSessionRegistry>,
) -> ImportStats {
    import_codex_filtered_chunk(
        server,
        path,
        input,
        state,
        workspace,
        event_sink,
        processor_registry,
        is_startup_context_event,
    )
}

#[allow(clippy::too_many_arguments)]
fn import_codex_filtered_chunk(
    server: &str,
    path: &Path,
    input: &str,
    state: &mut TranscriptState,
    workspace: Option<&WorkspaceFilter>,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
    processor_registry: Option<&ProcessorSessionRegistry>,
    include_event: impl Fn(&AgentEvent) -> bool,
) -> ImportStats {
    let mut stats = ImportStats::default();
    if is_agent_feed_internal_transcript_for_registry(input, Some(state), processor_registry) {
        debug!(
            path = %path.display(),
            "codex transcript chunk skipped agent-feed internal processor session"
        );
        if let Some(registry) = processor_registry {
            let session_id =
                codex_session_id_from_input(input).or_else(|| state.session_id.clone());
            registry.record_processor_session_skipped(session_id.as_deref());
        }
        return stats;
    }
    for (index, line) in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(err) => {
                debug!(
                    path = %path.display(),
                    line = index + 1,
                    error = %err,
                    "codex startup context line ignored"
                );
                continue;
            }
        };
        let Some(mut event) = normalize_transcript_value(value, state, Some(path)) else {
            continue;
        };
        if processor_registry.is_some_and(|registry| registry.drop_processor_event(&event)) {
            stats.filtered += 1;
            continue;
        }
        if !include_event(&event) {
            continue;
        }
        mark_startup_context_event(&mut event);
        if !event_matches_workspace(&event, workspace, path, "codex") {
            stats.filtered += 1;
            continue;
        }
        let body = match serde_json::to_string(&event) {
            Ok(body) => body,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "codex startup context event encode failed"
                );
                continue;
            }
        };
        if let Err(err) = post_json(server, "/ingest/codex/jsonl", &body) {
            warn!(
                path = %path.display(),
                %server,
                error = %err,
                "codex startup context event post failed"
            );
            continue;
        }
        if let Some(sender) = event_sink
            && let Err(err) = sender.send(event.clone())
        {
            warn!(
                path = %path.display(),
                event_id = %event.id,
                error = %err,
                "codex startup context publish queue send failed"
            );
        }
        stats.imported += 1;
    }
    stats
}

#[cfg(test)]
fn warm_codex_state_from_sample(path: &Path, state: &mut TranscriptState) -> Result<(), CliError> {
    let len = fs::metadata(path)?.len();
    warm_codex_state_from_sample_at(path, len, state)
}

fn warm_codex_state_from_sample_at(
    path: &Path,
    end_offset: u64,
    state: &mut TranscriptState,
) -> Result<(), CliError> {
    let sample = transcript_sample_at(path, STARTUP_STATE_SAMPLE_BYTES, end_offset)?;
    warm_codex_state(path, &sample, state);
    Ok(())
}

fn warm_codex_state(path: &Path, input: &str, state: &mut TranscriptState) {
    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let envelope_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(envelope_type, "session_meta" | "turn_context") {
            continue;
        }
        let _ = normalize_transcript_value(value, state, Some(path));
    }
}

fn watch_claude_active_sessions(
    server: &str,
    projects_dir: &Path,
    limit: usize,
    poll_ms: u64,
    workspace: Option<&WorkspaceFilter>,
    include_history: bool,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
) -> Result<(), CliError> {
    let mut watchers = Vec::new();
    let mut seen = HashSet::<PathBuf>::new();
    let mut reported_empty = false;
    let rediscover_every = Duration::from_secs(5);
    let mut last_discovery = Instant::now() - rediscover_every;

    loop {
        if last_discovery.elapsed() >= rediscover_every {
            let paths = active_claude_session_paths(projects_dir, limit, workspace)?;
            for path in paths {
                if !seen.insert(path.clone()) {
                    continue;
                }
                let (state, stats, offset) = initialize_claude_watcher(
                    &path,
                    server,
                    workspace,
                    include_history,
                    event_sink,
                )?;
                info!(
                    path = %path.display(),
                    initial_events = stats.imported,
                    filtered_events = stats.filtered,
                    offset,
                    poll_ms,
                    workspace = workspace.map(WorkspaceFilter::display).unwrap_or_else(|| "all".to_string()),
                    "claude stream watcher discovered"
                );
                print_watch_started("claude stream", &path, stats);
                post_capture_health(server, "claude", "claude.stream-json", &path);
                post_capture_watch_status(CaptureWatchPost {
                    server,
                    agent: "claude",
                    adapter: "claude.stream-json",
                    path: &path,
                    state: "watching",
                    stats,
                    offset,
                    file_len: offset,
                    poll_ms,
                    workspace,
                });
                watchers.push(ClaudeWatcher {
                    path,
                    offset,
                    state,
                    pending: String::new(),
                    last_status_at: Instant::now(),
                    total_stats: stats,
                });
            }

            if watchers.is_empty() && !reported_empty {
                println!(
                    "claude capture: no active sessions found for {}; still watching for new sessions",
                    capture_scope_message(workspace)
                );
                reported_empty = true;
            } else if !watchers.is_empty() {
                reported_empty = false;
            }
            last_discovery = Instant::now();
        }

        poll_claude_watchers(server, &mut watchers, poll_ms, workspace, event_sink)?;
        std::thread::sleep(Duration::from_millis(poll_ms.max(100)));
    }
}

fn watch_claude_sessions(
    server: &str,
    paths: &[PathBuf],
    poll_ms: u64,
    workspace: Option<&WorkspaceFilter>,
    include_history: bool,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
) -> Result<(), CliError> {
    let mut watchers = Vec::new();
    for path in paths {
        let (state, stats, offset) =
            initialize_claude_watcher(path, server, workspace, include_history, event_sink)?;
        info!(
            path = %path.display(),
            initial_events = stats.imported,
            filtered_events = stats.filtered,
            offset,
            poll_ms,
            workspace = workspace.map(WorkspaceFilter::display).unwrap_or_else(|| "all".to_string()),
            "claude stream watcher started"
        );
        print_watch_started("claude stream", path, stats);
        post_capture_health(server, "claude", "claude.stream-json", path);
        post_capture_watch_status(CaptureWatchPost {
            server,
            agent: "claude",
            adapter: "claude.stream-json",
            path,
            state: "watching",
            stats,
            offset,
            file_len: offset,
            poll_ms,
            workspace,
        });
        watchers.push(ClaudeWatcher {
            path: path.clone(),
            offset,
            state,
            pending: String::new(),
            last_status_at: Instant::now(),
            total_stats: stats,
        });
    }

    loop {
        std::thread::sleep(Duration::from_millis(poll_ms.max(100)));
        poll_claude_watchers(server, &mut watchers, poll_ms, workspace, event_sink)?;
    }
}

fn poll_claude_watchers(
    server: &str,
    watchers: &mut [ClaudeWatcher],
    poll_ms: u64,
    workspace: Option<&WorkspaceFilter>,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
) -> Result<(), CliError> {
    for watcher in watchers {
        let len = fs::metadata(&watcher.path)?.len();
        if len < watcher.offset {
            warn!(
                path = %watcher.path.display(),
                previous_offset = watcher.offset,
                len,
                "claude stream shrank; resetting watcher offset"
            );
            watcher.offset = 0;
            watcher.pending.clear();
            watcher.total_stats = ImportStats::default();
            post_capture_watch_status(CaptureWatchPost {
                server,
                agent: "claude",
                adapter: "claude.stream-json",
                path: &watcher.path,
                state: "truncated",
                stats: ImportStats::default(),
                offset: watcher.offset,
                file_len: len,
                poll_ms,
                workspace,
            });
            watcher.last_status_at = Instant::now();
        }
        if len <= watcher.offset {
            if watcher.last_status_at.elapsed() >= CAPTURE_STATUS_HEARTBEAT {
                post_capture_watch_status(CaptureWatchPost {
                    server,
                    agent: "claude",
                    adapter: "claude.stream-json",
                    path: &watcher.path,
                    state: "watching",
                    stats: watcher.total_stats,
                    offset: watcher.offset,
                    file_len: len,
                    poll_ms,
                    workspace,
                });
                watcher.last_status_at = Instant::now();
            }
            continue;
        }
        let mut file = File::open(&watcher.path)?;
        file.seek(SeekFrom::Start(watcher.offset))?;
        let mut chunk = String::new();
        file.read_to_string(&mut chunk)?;
        watcher.offset = len;
        watcher.pending.push_str(&chunk);
        let complete = split_complete_jsonl(&mut watcher.pending);
        let stats = import_claude_chunk(
            server,
            &watcher.path,
            &complete,
            &mut watcher.state,
            workspace,
            event_sink,
        );
        if stats.imported > 0 || stats.filtered > 0 {
            watcher.total_stats.add(stats);
            info!(
                path = %watcher.path.display(),
                events = stats.imported,
                filtered_events = stats.filtered,
                offset = watcher.offset,
                poll_ms,
                "claude stream appended events imported"
            );
            print_watch_appended("claude stream", &watcher.path, stats);
        }
        post_capture_watch_status(CaptureWatchPost {
            server,
            agent: "claude",
            adapter: "claude.stream-json",
            path: &watcher.path,
            state: "appended",
            stats: watcher.total_stats,
            offset: watcher.offset,
            file_len: len,
            poll_ms,
            workspace,
        });
        watcher.last_status_at = Instant::now();
    }
    Ok(())
}

fn import_claude_stream_input(
    server: &str,
    path: &Path,
    input: &str,
    workspace: Option<&WorkspaceFilter>,
) -> ImportStats {
    if is_agent_feed_internal_transcript(input) {
        debug!(
            path = %path.display(),
            "claude stream import skipped agent-feed internal processor session"
        );
        return ImportStats::default();
    }
    let mut state = ClaudeState::default();
    import_claude_chunk(server, path, input, &mut state, workspace, None)
}

fn import_claude_chunk(
    server: &str,
    path: &Path,
    input: &str,
    state: &mut ClaudeState,
    workspace: Option<&WorkspaceFilter>,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
) -> ImportStats {
    let mut stats = ImportStats::default();
    if is_agent_feed_internal_transcript(input) {
        debug!(
            path = %path.display(),
            "claude stream chunk skipped agent-feed internal processor session"
        );
        return stats;
    }
    for (index, line) in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    line = index + 1,
                    error = %err,
                    "claude stream parse failed during import"
                );
                eprintln!(
                    "agent-feed: failed to parse claude stream line {} in {}: {err}",
                    index + 1,
                    path.display()
                );
                continue;
            }
        };

        let Some(event) = normalize_stream_value(value, state, Some(path)) else {
            continue;
        };
        if !event_matches_workspace(&event, workspace, path, "claude") {
            stats.filtered += 1;
            continue;
        }
        let body = match serde_json::to_string(&event) {
            Ok(body) => body,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "claude event encode failed"
                );
                eprintln!(
                    "agent-feed: failed to encode claude event from {}: {err}",
                    path.display()
                );
                continue;
            }
        };
        if let Err(err) = post_json(server, "/ingest/claude/stream-json", &body) {
            warn!(
                path = %path.display(),
                %server,
                error = %err,
                "claude event post failed"
            );
            eprintln!(
                "agent-feed: failed to post claude event from {}: {err}",
                path.display()
            );
            continue;
        }
        debug!(
            path = %path.display(),
            index,
            event_id = %event.id,
            kind = ?event.kind,
            severity = ?event.severity,
            "claude event outgoing"
        );
        if let Some(sender) = event_sink
            && let Err(err) = sender.send(event.clone())
        {
            warn!(
                path = %path.display(),
                event_id = %event.id,
                error = %err,
                "claude event publish queue send failed"
            );
        }
        stats.imported += 1;
    }
    stats
}

fn initialize_claude_watcher(
    path: &Path,
    server: &str,
    workspace: Option<&WorkspaceFilter>,
    include_history: bool,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
) -> Result<(ClaudeState, ImportStats, u64), CliError> {
    let initial_len = fs::metadata(path)?.len();
    let mut state = ClaudeState::default();
    let stats = if include_history {
        let input = transcript_prefix(path, initial_len)?;
        import_claude_chunk(server, path, &input, &mut state, workspace, event_sink)
    } else {
        warm_claude_state_from_sample_at(path, initial_len, &mut state)?;
        let context = transcript_suffix_at(path, STARTUP_CONTEXT_TAIL_BYTES, initial_len)?;
        import_claude_context_chunk(server, path, &context, &mut state, workspace, event_sink)
    };
    Ok((state, stats, initial_len))
}

fn import_claude_context_chunk(
    server: &str,
    path: &Path,
    input: &str,
    state: &mut ClaudeState,
    workspace: Option<&WorkspaceFilter>,
    event_sink: Option<&mpsc::Sender<AgentEvent>>,
) -> ImportStats {
    let mut stats = ImportStats::default();
    if is_agent_feed_internal_transcript(input) {
        debug!(
            path = %path.display(),
            "claude startup context skipped agent-feed internal processor session"
        );
        return stats;
    }
    for (index, line) in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(err) => {
                debug!(
                    path = %path.display(),
                    line = index + 1,
                    error = %err,
                    "claude startup context line ignored"
                );
                continue;
            }
        };
        let Some(mut event) = normalize_stream_value(value, state, Some(path)) else {
            continue;
        };
        if !is_startup_context_event(&event) {
            continue;
        }
        mark_startup_context_event(&mut event);
        if !event_matches_workspace(&event, workspace, path, "claude") {
            stats.filtered += 1;
            continue;
        }
        let body = match serde_json::to_string(&event) {
            Ok(body) => body,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "claude startup context event encode failed"
                );
                continue;
            }
        };
        if let Err(err) = post_json(server, "/ingest/claude/stream-json", &body) {
            warn!(
                path = %path.display(),
                %server,
                error = %err,
                "claude startup context event post failed"
            );
            continue;
        }
        if let Some(sender) = event_sink
            && let Err(err) = sender.send(event.clone())
        {
            warn!(
                path = %path.display(),
                event_id = %event.id,
                error = %err,
                "claude startup context publish queue send failed"
            );
        }
        stats.imported += 1;
    }
    stats
}

fn warm_claude_state_from_sample_at(
    path: &Path,
    end_offset: u64,
    state: &mut ClaudeState,
) -> Result<(), CliError> {
    let sample = transcript_sample_at(path, STARTUP_STATE_SAMPLE_BYTES, end_offset)?;
    warm_claude_state(path, &sample, state);
    Ok(())
}

fn warm_claude_state(path: &Path, input: &str, state: &mut ClaudeState) {
    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let _ = normalize_stream_value(value, state, Some(path));
    }
}

fn split_complete_jsonl(buffer: &mut String) -> String {
    if buffer.ends_with('\n') {
        return std::mem::take(buffer);
    }

    let Some((complete, pending)) = buffer.rsplit_once('\n') else {
        return String::new();
    };
    let complete = format!("{complete}\n");
    let pending = pending.to_string();
    *buffer = pending;
    complete
}

fn is_startup_context_event(event: &AgentEvent) -> bool {
    matches!(
        event.kind,
        agent_feed_core::EventKind::SessionStart
            | agent_feed_core::EventKind::TurnStart
            | agent_feed_core::EventKind::CommandExec
            | agent_feed_core::EventKind::ToolStart
            | agent_feed_core::EventKind::McpCall
            | agent_feed_core::EventKind::WebSearch
            | agent_feed_core::EventKind::AgentMessage
            | agent_feed_core::EventKind::AdapterHealth
    )
}

fn mark_startup_context_event(event: &mut AgentEvent) {
    if !event.tags.iter().any(|tag| tag == STARTUP_CONTEXT_TAG) {
        event.tags.push(STARTUP_CONTEXT_TAG.to_string());
    }
}

#[derive(Debug)]
struct CodexWatcher {
    path: PathBuf,
    offset: u64,
    state: TranscriptState,
    pending: String,
    last_status_at: Instant,
    internal_processor: bool,
    total_stats: ImportStats,
}

#[derive(Debug)]
struct ClaudeWatcher {
    path: PathBuf,
    offset: u64,
    state: ClaudeState,
    pending: String,
    last_status_at: Instant,
    total_stats: ImportStats,
}

fn active_codex_session_paths(
    history: &Path,
    sessions_dir: &Path,
    limit: usize,
    workspace: Option<&WorkspaceFilter>,
    processor_registry: Option<&ProcessorSessionRegistry>,
) -> Result<Vec<PathBuf>, CliError> {
    active_codex_session_paths_with_process_ids(
        history,
        sessions_dir,
        limit,
        workspace,
        processor_registry,
        &codex_process_session_ids(),
    )
}

fn active_codex_session_paths_with_process_ids(
    history: &Path,
    sessions_dir: &Path,
    limit: usize,
    workspace: Option<&WorkspaceFilter>,
    processor_registry: Option<&ProcessorSessionRegistry>,
    process_session_ids: &[String],
) -> Result<Vec<PathBuf>, CliError> {
    if limit == 0 || !sessions_dir.exists() {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    for session_id in process_session_ids {
        if let Some(path) = find_session_path(sessions_dir, session_id)? {
            candidates.push(path);
        }
    }
    candidates.extend(jsonl_files_by_mtime(sessions_dir)?);
    if history.exists() {
        let input = fs::read_to_string(history)?;
        let mut seen_history = HashSet::new();
        for line in input.lines().rev() {
            let value = serde_json::from_str::<Value>(line)?;
            let Some(session_id) = value.get("session_id").and_then(Value::as_str) else {
                continue;
            };
            if !seen_history.insert(session_id.to_string()) {
                continue;
            }
            if let Some(path) = find_session_path(sessions_dir, session_id)? {
                candidates.push(path);
            }
            if seen_history.len() >= limit.saturating_mul(4).max(limit) {
                break;
            }
        }
    }

    let mut seen = HashSet::<PathBuf>::new();
    let mut paths = Vec::new();
    for path in candidates {
        if !seen.insert(path.clone()) {
            continue;
        };
        if let Some(registry) = processor_registry
            && let Some(session_id) = codex_session_id_from_path_or_sample(&path)?
            && registry.is_known_session_id(&session_id)
        {
            registry.record_processor_session_skipped(Some(&session_id));
            debug!(
                path = %path.display(),
                session_id,
                "codex active session skipped by processor registry"
            );
            continue;
        }
        if is_agent_feed_internal_transcript_path(&path, processor_registry)? {
            if let Some(registry) = processor_registry {
                let session_id = codex_session_id_from_path_or_sample(&path)?;
                registry.record_processor_session_skipped(session_id.as_deref());
            }
            debug!(
                path = %path.display(),
                "codex active session skipped agent-feed internal processor session"
            );
            continue;
        }
        if session_matches_workspace(&path, workspace, collect_codex_events)? {
            paths.push(path);
        } else if let Some(workspace) = workspace {
            debug!(
                path = %path.display(),
                workspace = %workspace.display(),
                "codex active session skipped by workspace filter"
            );
        };
        if paths.len() >= limit {
            break;
        }
    }
    Ok(paths)
}

fn codex_process_session_ids() -> Vec<String> {
    let output = ProcessCommand::new("ps")
        .args(["-eo", "args="])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_codex_resume_session_id)
        .collect()
}

fn parse_codex_resume_session_id(line: &str) -> Option<String> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    for window in parts.windows(2) {
        if window[0] == "resume" && is_uuid_like_session_id(window[1]) {
            return Some(window[1].to_string());
        }
    }
    None
}

fn codex_session_id_from_path_or_sample(path: &Path) -> Result<Option<String>, CliError> {
    let sample = transcript_sample(path, STARTUP_STATE_SAMPLE_BYTES)?;
    if let Some(session_id) = codex_session_id_from_input(&sample) {
        return Ok(Some(session_id));
    }
    Ok(codex_session_id_from_path(path))
}

fn codex_session_id_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let session_id = stem.strip_prefix("rollout-").unwrap_or(stem);
    let session_id = session_id
        .char_indices()
        .rev()
        .find_map(|(index, _)| {
            let candidate = &session_id[index..];
            is_uuid_like_session_id(candidate).then_some(candidate)
        })
        .unwrap_or(session_id);
    if session_id.trim().is_empty() {
        None
    } else {
        Some(session_id.to_string())
    }
}

fn is_uuid_like_session_id(value: &str) -> bool {
    value.len() == 36
        && value
            .chars()
            .enumerate()
            .all(|(index, character)| match index {
                8 | 13 | 18 | 23 => character == '-',
                _ => character.is_ascii_hexdigit(),
            })
}

fn codex_session_id_from_input(input: &str) -> Option<String> {
    for line in input.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(session_id) = codex_session_id_from_value(&value) {
            return Some(session_id);
        }
    }
    None
}

fn codex_session_id_from_value(value: &Value) -> Option<String> {
    let payload = value.get("payload").unwrap_or(value);
    value
        .get("session_id")
        .and_then(Value::as_str)
        .or_else(|| value.get("id").and_then(Value::as_str))
        .or_else(|| payload.get("session_id").and_then(Value::as_str))
        .or_else(|| payload.get("id").and_then(Value::as_str))
        .filter(|session_id| !session_id.trim().is_empty())
        .map(str::to_string)
}

fn active_claude_session_paths(
    root: &Path,
    limit: usize,
    workspace: Option<&WorkspaceFilter>,
) -> Result<Vec<PathBuf>, CliError> {
    if limit == 0 || !root.exists() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for path in jsonl_files_by_mtime(root)? {
        if is_agent_feed_internal_transcript_path(&path, None)? {
            debug!(
                path = %path.display(),
                "claude active session skipped agent-feed internal processor session"
            );
            continue;
        }
        if session_matches_workspace(&path, workspace, collect_claude_events)? {
            paths.push(path);
        } else if let Some(workspace) = workspace {
            debug!(
                path = %path.display(),
                workspace = %workspace.display(),
                "claude active session skipped by workspace filter"
            );
        }
        if paths.len() >= limit {
            break;
        }
    }
    Ok(paths)
}

const AGENT_FEED_INTERNAL_TRANSCRIPT_SAMPLE_BYTES: usize = 256 * 1024;

fn is_agent_feed_internal_transcript_path(
    path: &Path,
    processor_registry: Option<&ProcessorSessionRegistry>,
) -> Result<bool, CliError> {
    let sample = transcript_sample(path, AGENT_FEED_INTERNAL_TRANSCRIPT_SAMPLE_BYTES)?;
    Ok(is_agent_feed_internal_transcript_for_registry(
        &sample,
        None,
        processor_registry,
    ))
}

fn transcript_sample(path: &Path, max_bytes: usize) -> Result<String, CliError> {
    let len = fs::metadata(path)?.len();
    transcript_sample_at(path, max_bytes, len)
}

fn transcript_sample_at(
    path: &Path,
    max_bytes: usize,
    end_offset: u64,
) -> Result<String, CliError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len().min(end_offset);
    if len <= max_bytes as u64 {
        return transcript_prefix(path, len);
    }

    let mut prefix = vec![0u8; max_bytes / 2];
    let prefix_len = file.read(&mut prefix)?;
    prefix.truncate(prefix_len);

    file.seek(SeekFrom::Start(len.saturating_sub((max_bytes / 2) as u64)))?;
    let mut suffix = Vec::with_capacity(max_bytes / 2);
    file.take((max_bytes / 2) as u64).read_to_end(&mut suffix)?;

    let mut sample = String::from_utf8_lossy(&prefix).to_string();
    sample.push('\n');
    sample.push_str(&String::from_utf8_lossy(&suffix));
    Ok(sample)
}

#[cfg(test)]
fn transcript_suffix(path: &Path, max_bytes: usize) -> Result<String, CliError> {
    let len = fs::metadata(path)?.len();
    transcript_suffix_at(path, max_bytes, len)
}

fn transcript_suffix_at(
    path: &Path,
    max_bytes: usize,
    end_offset: u64,
) -> Result<String, CliError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len().min(end_offset);
    if len == 0 || max_bytes == 0 {
        return Ok(String::new());
    }

    let start = len.saturating_sub(max_bytes as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(len - start).read_to_end(&mut bytes)?;
    let mut sample = String::from_utf8_lossy(&bytes).to_string();

    if start > 0 {
        match sample.find('\n') {
            Some(index) => sample = sample[index + 1..].to_string(),
            None => sample.clear(),
        }
    }

    if !sample.ends_with('\n') {
        match sample.rfind('\n') {
            Some(index) => sample.truncate(index + 1),
            None => sample.clear(),
        }
    }

    Ok(sample)
}

fn transcript_prefix(path: &Path, end_offset: u64) -> Result<String, CliError> {
    let file = File::open(path)?;
    let len = file.metadata()?.len().min(end_offset);
    let mut bytes = Vec::new();
    file.take(len).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

#[derive(Clone, Debug, Default)]
struct InternalTranscriptSignals {
    saw_internal_prompt: bool,
    saw_external_activity: bool,
    session_id: Option<String>,
    cwd: Option<String>,
}

fn is_agent_feed_internal_transcript(input: &str) -> bool {
    let signals = internal_transcript_signals(input, None);
    if signals
        .cwd
        .as_deref()
        .is_some_and(is_default_processor_work_dir)
    {
        return true;
    }
    signals.saw_internal_prompt && !signals.saw_external_activity
}

fn is_agent_feed_internal_transcript_for_registry(
    input: &str,
    state: Option<&TranscriptState>,
    processor_registry: Option<&ProcessorSessionRegistry>,
) -> bool {
    let signals = internal_transcript_signals(input, state);
    if signals
        .cwd
        .as_deref()
        .is_some_and(is_default_processor_work_dir)
    {
        return true;
    }
    let Some(registry) = processor_registry else {
        return signals.saw_internal_prompt && !signals.saw_external_activity;
    };
    if let Some(session_id) = signals.session_id.as_deref()
        && registry.is_known_session_id(session_id)
    {
        return true;
    }
    if !signals.saw_internal_prompt || signals.saw_external_activity {
        return false;
    }
    signals
        .cwd
        .as_deref()
        .is_some_and(|cwd| registry.owns_cwd(cwd))
}

fn confirmed_agent_feed_internal_processor_chunk(
    input: &str,
    state: &TranscriptState,
    processor_registry: Option<&ProcessorSessionRegistry>,
) -> bool {
    let signals = internal_transcript_signals(input, Some(state));
    if signals
        .cwd
        .as_deref()
        .is_some_and(is_default_processor_work_dir)
    {
        return true;
    }
    if let Some(registry) = processor_registry {
        if let Some(session_id) = signals.session_id.as_deref()
            && registry.is_known_session_id(session_id)
        {
            return true;
        }
        return signals
            .cwd
            .as_deref()
            .is_some_and(|cwd| registry.owns_cwd(cwd));
    }
    signals.session_id.is_some() && signals.saw_internal_prompt && !signals.saw_external_activity
}

fn is_default_processor_work_dir(cwd: &str) -> bool {
    let cwd = clean_path(&expand_home_path(PathBuf::from(cwd)));
    let default_store = PathBuf::from(default_summary_memory_store_string());
    let default_work_dir = clean_path(&expand_home_path(summary_memory_work_dir(&default_store)));
    cwd == default_work_dir || cwd.starts_with(&default_work_dir)
}

fn internal_transcript_signals(
    input: &str,
    state: Option<&TranscriptState>,
) -> InternalTranscriptSignals {
    let mut signals = InternalTranscriptSignals {
        session_id: state.and_then(|state| state.session_id.clone()),
        cwd: state.and_then(|state| state.active_cwd.clone().or_else(|| state.cwd.clone())),
        ..InternalTranscriptSignals::default()
    };

    for line in input.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        signals.saw_external_activity |= transcript_value_has_external_activity(&value);
        if signals.session_id.is_none() {
            signals.session_id = codex_session_id_from_value(&value);
        }
        if signals.cwd.is_none() {
            signals.cwd = codex_cwd_from_value(&value);
        }
        if line.contains(INTERNAL_SUMMARIZER_MARKER) {
            signals.saw_internal_prompt |= transcript_user_payload_contains_internal_prompt(&value);
        }
    }

    signals
}

fn codex_cwd_from_value(value: &Value) -> Option<String> {
    let payload = value.get("payload").unwrap_or(value);
    value
        .get("cwd")
        .and_then(Value::as_str)
        .or_else(|| value.get("workdir").and_then(Value::as_str))
        .or_else(|| payload.get("cwd").and_then(Value::as_str))
        .or_else(|| payload.get("workdir").and_then(Value::as_str))
        .filter(|cwd| !cwd.trim().is_empty())
        .map(str::to_string)
}

fn transcript_user_payload_contains_internal_prompt(value: &Value) -> bool {
    let top_type = value.get("type").and_then(Value::as_str);
    if top_type == Some("user")
        && value
            .get("message")
            .is_some_and(value_contains_internal_prompt)
    {
        return true;
    }
    let Some(payload) = value.get("payload") else {
        return false;
    };
    if payload.get("type").and_then(Value::as_str) == Some("user_message")
        && payload
            .get("message")
            .is_some_and(value_contains_internal_prompt)
    {
        return true;
    }
    false
}

fn value_contains_internal_prompt(value: &Value) -> bool {
    let mut text = String::new();
    collect_value_text(value, &mut text);
    text_is_internal_summarizer_prompt(&text)
}

fn collect_value_text(value: &Value, output: &mut String) {
    match value {
        Value::String(value) => {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(value);
        }
        Value::Array(values) => {
            for value in values {
                collect_value_text(value, output);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_value_text(value, output);
            }
        }
        _ => {}
    }
}

fn text_is_internal_summarizer_prompt(text: &str) -> bool {
    if !text.contains(INTERNAL_SUMMARIZER_MARKER) {
        return false;
    }

    let story_prompt = text
        .contains("Return one JSON object with headline, deck, lower_third, chips")
        && text.contains("feed=")
        && text.contains("mode=")
        && text.contains("stories:");
    let memory_prompt = text
        .contains("You are the private local headline memory for one agent feed")
        && text.contains("summary_memory_key=")
        && text.contains("prior_memory_digest=")
        && text.contains("prior_semantic_fingerprint=");
    let external_story_prompt =
        story_prompt && text.contains("Set publish=false") && text.contains("recent_published");
    let image_prompt = text.contains("Either return")
        && text.contains("\"image\": null")
        && text.contains("headline=")
        && text.contains("deck=")
        && text.contains("chips=");

    (memory_prompt && story_prompt) || external_story_prompt || image_prompt
}

fn transcript_value_has_external_activity(value: &Value) -> bool {
    let top_type = value.get("type").and_then(Value::as_str);
    let payload = value.get("payload").unwrap_or(value);
    let payload_type = payload.get("type").and_then(Value::as_str).or(top_type);

    matches!(
        payload_type,
        Some(
            "function_call"
                | "function_call_output"
                | "exec_command_end"
                | "tool_call"
                | "tool_result"
                | "command.exec"
                | "command_exec"
                | "file_change"
                | "file.changed"
        )
    ) || payload.get("command").is_some()
        || payload.get("call_id").is_some()
}

fn session_matches_workspace(
    path: &Path,
    workspace: Option<&WorkspaceFilter>,
    collect: fn(&Path, Option<&WorkspaceFilter>) -> Result<CollectedEvents, CliError>,
) -> Result<bool, CliError> {
    if workspace.is_none() {
        return Ok(true);
    }
    Ok(!collect(path, workspace)?.events.is_empty())
}

fn jsonl_files_by_mtime(root: &Path) -> Result<Vec<PathBuf>, CliError> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
                let modified = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok();
                files.push((modified, path));
            }
        }
    }
    files.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

fn find_session_path(root: &Path, session_id: &str) -> Result<Option<PathBuf>, CliError> {
    if !root.exists() {
        return Ok(None);
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.contains(session_id) && name.ends_with(".jsonl") {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

fn default_codex_history() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("history.jsonl")
}

fn default_codex_sessions_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("sessions")
}

fn default_claude_projects_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}

fn parse_agent_list(value: &str) -> HashSet<&str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|agent| matches!(*agent, "codex" | "claude"))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum DoctorCheckStatus {
    Ok,
    Warn,
    Error,
}

impl DoctorCheckStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Serialize)]
struct PublishDoctorCheck {
    name: &'static str,
    status: DoctorCheckStatus,
    detail: String,
}

#[derive(Debug, Serialize)]
struct PublishDoctorReport {
    status: DoctorCheckStatus,
    checks: Vec<PublishDoctorCheck>,
    next: String,
}

fn doctor_publish(
    server: &str,
    edge: &str,
    network_id: &str,
    auth_store: Option<PathBuf>,
) -> PublishDoctorReport {
    let mut checks = Vec::new();
    let mut next = None::<String>;

    let auth_session = match load_publish_session(auth_store, edge) {
        Ok(session) => {
            push_doctor_check(
                &mut checks,
                DoctorCheckStatus::Ok,
                "auth",
                format!(
                    "signed in as @{} · github:{} · expires {}",
                    session.login, session.github_user_id, session.expires_at
                ),
            );
            Some(session)
        }
        Err(err) => {
            push_doctor_check(
                &mut checks,
                DoctorCheckStatus::Error,
                "auth",
                err.to_string(),
            );
            set_doctor_next(
                &mut next,
                format!(
                    "run `agent-feed auth github --edge {}`",
                    edge.trim_end_matches('/')
                ),
            );
            None
        }
    };

    let local_status = match get_json::<StatusView>(server, "/api/status") {
        Ok(status) => {
            push_doctor_check(
                &mut checks,
                DoctorCheckStatus::Ok,
                "local server",
                format!("reachable at {server} · status {}", status.status),
            );
            Some(status)
        }
        Err(err) => {
            push_doctor_check(
                &mut checks,
                DoctorCheckStatus::Error,
                "local server",
                format!("not reachable at {server}: {err}"),
            );
            set_doctor_next(
                &mut next,
                "start `agent-feed serve --publish --feed workstation --workspace .`".to_string(),
            );
            None
        }
    };

    if let Some(status) = local_status.as_ref() {
        add_capture_doctor_check(&mut checks, &mut next, status);
        add_story_doctor_check(&mut checks, &mut next, status);
        add_publish_doctor_check(&mut checks, &mut next, status);
        add_edge_visibility_doctor_check(
            &mut checks,
            &mut next,
            edge,
            network_id,
            status.publish.as_ref(),
            auth_session.as_ref(),
        );
    } else {
        push_doctor_check(
            &mut checks,
            DoctorCheckStatus::Warn,
            "capture",
            "skipped because the local feed server is not reachable".to_string(),
        );
        push_doctor_check(
            &mut checks,
            DoctorCheckStatus::Warn,
            "story gate",
            "skipped because the local feed server is not reachable".to_string(),
        );
        push_doctor_check(
            &mut checks,
            DoctorCheckStatus::Warn,
            "publish",
            "skipped because the local feed server is not reachable".to_string(),
        );
        push_doctor_check(
            &mut checks,
            DoctorCheckStatus::Warn,
            "edge snapshot",
            "skipped until a local publisher reports its feed state".to_string(),
        );
    }

    let status = checks
        .iter()
        .map(|check| check.status)
        .max_by_key(|status| match status {
            DoctorCheckStatus::Ok => 0,
            DoctorCheckStatus::Warn => 1,
            DoctorCheckStatus::Error => 2,
        })
        .unwrap_or(DoctorCheckStatus::Ok);
    PublishDoctorReport {
        status,
        checks,
        next: next.unwrap_or_else(|| {
            if let Some(session) = auth_session.as_ref()
                && let Some(status) = local_status.as_ref()
                && let Some(publish) = status.publish.as_ref()
            {
                return format!(
                    "open https://feed.aberration.technology/{}/{}?feed_mode=discovery",
                    session.login, publish.feed
                );
            }
            "feed publish path looks healthy".to_string()
        }),
    }
}

fn add_capture_doctor_check(
    checks: &mut Vec<PublishDoctorCheck>,
    next: &mut Option<String>,
    status: &StatusView,
) {
    let watchers = status.capture_watchers.len();
    let sources = status.captured_sources.len();
    if watchers > 0 {
        let imported = status
            .capture_watchers
            .iter()
            .map(|watcher| watcher.imported_events)
            .sum::<usize>();
        push_doctor_check(
            checks,
            DoctorCheckStatus::Ok,
            "capture",
            format!(
                "{watchers} watcher{} · {sources} src · {imported} imported",
                plural(watchers)
            ),
        );
    } else if sources > 0 {
        push_doctor_check(
            checks,
            DoctorCheckStatus::Warn,
            "capture",
            format!("{sources} src reported, but no active transcript watchers"),
        );
        set_doctor_next(
            next,
            "restart `agent-feed serve --publish` so future agent transcript writes are watched"
                .to_string(),
        );
    } else {
        push_doctor_check(
            checks,
            DoctorCheckStatus::Warn,
            "capture",
            "no active capture watchers or sources".to_string(),
        );
        set_doctor_next(
            next,
            "start `agent-feed serve --publish --feed workstation --workspace .`, then continue work in codex or claude".to_string(),
        );
    }
}

fn add_story_doctor_check(
    checks: &mut Vec<PublishDoctorCheck>,
    next: &mut Option<String>,
    status: &StatusView,
) {
    if status.stored_bulletins > 0 || status.story.published_stories > 0 {
        push_doctor_check(
            checks,
            DoctorCheckStatus::Ok,
            "story gate",
            format!(
                "{} story/stories stored · {} published · {} rejected · {} deduped",
                status.stored_bulletins,
                status.story.published_stories,
                status.story.rejected_stories,
                status.story.deduped_stories
            ),
        );
    } else if status.stored_events > 0 {
        let detail = status
            .story
            .last_decision
            .as_ref()
            .map(|decision| {
                format!(
                    "{} {} · score {}/context {} · {}",
                    decision.agent,
                    decision.action,
                    decision.score,
                    decision.context_score,
                    decision.reason
                )
            })
            .unwrap_or_else(|| {
                "events are arriving; waiting for completion, test, edit, or incident context"
                    .to_string()
            });
        push_doctor_check(checks, DoctorCheckStatus::Warn, "story gate", detail);
        set_doctor_next(
            next,
            "keep working until a completion, test, edit, or incident settles; inspect `agent-feed status` for gate details".to_string(),
        );
    } else {
        push_doctor_check(
            checks,
            DoctorCheckStatus::Warn,
            "story gate",
            "no events observed since this feed process started".to_string(),
        );
        set_doctor_next(
            next,
            "continue or restart codex/claude after `agent-feed serve --publish` is running"
                .to_string(),
        );
    }
}

fn add_publish_doctor_check(
    checks: &mut Vec<PublishDoctorCheck>,
    next: &mut Option<String>,
    status: &StatusView,
) {
    let Some(publish) = status.publish.as_ref() else {
        push_doctor_check(
            checks,
            DoctorCheckStatus::Warn,
            "publish",
            "local server is not running with --publish".to_string(),
        );
        set_doctor_next(
            next,
            "restart with `agent-feed serve --publish --feed workstation --workspace .`"
                .to_string(),
        );
        return;
    };

    let check_status = match publish.state.as_str() {
        "published" | "present" => DoctorCheckStatus::Ok,
        "starting" | "queued" | "publishing" | "skipped" => DoctorCheckStatus::Warn,
        "degraded" | "error" => DoctorCheckStatus::Error,
        _ => DoctorCheckStatus::Warn,
    };
    let mut detail = format!(
        "{} · feed {} · pending {} · last batch {} capsule{}",
        publish.state,
        publish.feed,
        publish.pending_stories,
        publish.last_batch_capsules,
        plural(publish.last_batch_capsules)
    );
    if publish.last_edge_feeds > 0 || publish.last_edge_headlines > 0 {
        let _ = write!(
            detail,
            " · edge accepted {} · {} feeds · {} headlines",
            publish.last_edge_accepted, publish.last_edge_feeds, publish.last_edge_headlines
        );
    }
    if publish.processor_sessions > 0
        || publish.processor_events_dropped > 0
        || publish.processor_sessions_skipped > 0
        || publish.ambiguous_internal_candidates > 0
    {
        let _ = write!(
            detail,
            " · processor guard {} session{} · {} dropped · {} skipped · {} ambiguous",
            publish.processor_sessions,
            plural(publish.processor_sessions),
            publish.processor_events_dropped,
            publish.processor_sessions_skipped,
            publish.ambiguous_internal_candidates
        );
    }
    if let Some(error) = publish.last_error.as_deref() {
        let _ = write!(detail, " · {error}");
    } else if let Some(extra) = publish.detail.as_deref() {
        let _ = write!(detail, " · {extra}");
    }
    push_doctor_check(checks, check_status, "publish", detail);
    match publish.state.as_str() {
        "degraded" | "error" => set_doctor_next(
            next,
            "check github auth and edge reachability, then rerun `agent-feed doctor publish`"
                .to_string(),
        ),
        "starting" | "queued" | "publishing" => set_doctor_next(
            next,
            "wait for the next publish interval or a high-priority story, then rerun `agent-feed doctor publish`".to_string(),
        ),
        "skipped" => set_doctor_next(
            next,
            "keep working until the summarizer sees a meaningful headline change".to_string(),
        ),
        _ => {}
    }
}

fn add_edge_visibility_doctor_check(
    checks: &mut Vec<PublishDoctorCheck>,
    next: &mut Option<String>,
    edge: &str,
    network_id: &str,
    publish: Option<&PublishStatusView>,
    auth_session: Option<&GithubAuthSession>,
) {
    let Some(publish) = publish else {
        push_doctor_check(
            checks,
            DoctorCheckStatus::Warn,
            "edge snapshot",
            "skipped because local publish state is absent".to_string(),
        );
        return;
    };
    let path = format!(
        "/network/snapshot?network={}",
        edge_network_query_value(network_id)
    );
    let snapshot = match get_edge_json(edge, &path) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            push_doctor_check(
                checks,
                DoctorCheckStatus::Error,
                "edge snapshot",
                format!("could not load {path}: {err}"),
            );
            set_doctor_next(
                next,
                "verify edge health at https://api.feed.aberration.technology/healthz".to_string(),
            );
            return;
        }
    };
    let feeds = snapshot
        .get("feeds")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let headlines = snapshot
        .get("headlines")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let expected_login = publish
        .publisher
        .as_deref()
        .or_else(|| auth_session.map(|session| session.login.as_str()))
        .map(clean_github_login);
    let feed_visible = snapshot
        .get("feeds")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                value_str(item, "label") == Some(publish.feed.as_str())
                    && publisher_matches(item, expected_login.as_deref())
            })
        });
    let visible_headlines = snapshot
        .get("headlines")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    value_str(item, "feed_label") == Some(publish.feed.as_str())
                        && publisher_matches(item, expected_login.as_deref())
                })
                .count()
        })
        .unwrap_or_default();
    if feed_visible {
        let status = if publish.last_batch_capsules > 0 && visible_headlines == 0 {
            DoctorCheckStatus::Warn
        } else {
            DoctorCheckStatus::Ok
        };
        push_doctor_check(
            checks,
            status,
            "edge snapshot",
            format!(
                "feed visible · {visible_headlines} matching headline{} · network has {feeds} feeds and {headlines} headlines",
                plural(visible_headlines)
            ),
        );
        if status == DoctorCheckStatus::Warn {
            set_doctor_next(
                next,
                "wait for the next accepted capsule batch; the feed is visible but no matching headline is in the edge snapshot yet".to_string(),
            );
        }
    } else {
        push_doctor_check(
            checks,
            DoctorCheckStatus::Warn,
            "edge snapshot",
            format!(
                "feed `{}` is not visible in the edge snapshot yet · network has {feeds} feeds and {headlines} headlines",
                publish.feed
            ),
        );
        set_doctor_next(
            next,
            "wait for feed presence registration or rerun `agent-feed serve --publish --feed <name>`"
                .to_string(),
        );
    }
}

fn push_doctor_check(
    checks: &mut Vec<PublishDoctorCheck>,
    status: DoctorCheckStatus,
    name: &'static str,
    detail: String,
) {
    checks.push(PublishDoctorCheck {
        name,
        status,
        detail,
    });
}

fn set_doctor_next(next: &mut Option<String>, value: String) {
    if next.is_none() {
        *next = Some(value);
    }
}

fn format_publish_doctor_report(report: &PublishDoctorReport) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "publish doctor: {}", report.status.as_str());
    for check in &report.checks {
        let _ = writeln!(
            output,
            "{}: {} · {}",
            check.name,
            check.status.as_str(),
            check.detail
        );
    }
    let _ = writeln!(output, "next: {}", report.next);
    output
}

fn edge_network_query_value(network_id: &str) -> &str {
    if network_id.trim().is_empty() || network_id == "agent-feed-mainnet" {
        "mainnet"
    } else {
        network_id
    }
}

fn p2p_capability_json(capability: &P2pDataPlaneCapability) -> Value {
    json!({
        "data_plane": capability.data_plane.as_str(),
        "available": capability.available,
        "production_default": capability.production_default,
        "publish_available": capability.publish_available,
        "subscribe_available": capability.subscribe_available,
        "reason": capability.reason,
        "next_step": capability.next_step,
        "protocols": capability.protocols,
        "transports": capability.transports,
    })
}

fn value_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn clean_github_login(login: &str) -> String {
    login.trim().trim_start_matches('@').to_ascii_lowercase()
}

fn publisher_matches(value: &Value, expected_login: Option<&str>) -> bool {
    let Some(expected_login) = expected_login else {
        return true;
    };
    value_str(value, "publisher_login")
        .map(clean_github_login)
        .is_some_and(|login| login == expected_login)
}

fn format_local_status(status: &StatusView) -> String {
    let mut output = String::new();
    let watchers = status.capture_watchers.len();
    let sources = status.captured_sources.len();
    let _ = writeln!(output, "feed status: {}", status.status);
    let _ = writeln!(output, "server: {}", status.bind);
    let _ = writeln!(
        output,
        "p2p: {}",
        if status.p2p_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    let _ = writeln!(
        output,
        "capture: {watchers} watcher{} · {sources} src",
        plural(watchers)
    );
    let _ = writeln!(
        output,
        "events: {} ingested · {} stored · {} dropped",
        status.ingested_events, status.stored_events, status.dropped_events
    );
    let _ = writeln!(
        output,
        "stories: {} emitted · {} stored",
        status.emitted_bulletins, status.stored_bulletins
    );
    if story_status_is_active(&status.story) {
        let _ = writeln!(
            output,
            "story gate: {} open · {} published · {} rejected · {} deduped",
            status.story.open_windows,
            status.story.published_stories,
            status.story.rejected_stories,
            status.story.deduped_stories
        );
        if let Some(decision) = &status.story.last_decision {
            let _ = writeln!(
                output,
                "last gate: {} {} · score {}/context {} · {}",
                decision.agent,
                decision.action,
                decision.score,
                decision.context_score,
                decision.reason
            );
        }
    }
    if let Some(kind) = &status.last_event_kind {
        let _ = writeln!(output, "last event: {kind}");
    }
    if status.last_bulletin_at.is_some() {
        let _ = writeln!(output, "last story: published");
    }
    if let Some(publish) = &status.publish {
        let _ = writeln!(
            output,
            "publish: {} · feed {} · pending {} · last batch {} capsule{}",
            publish.state,
            publish.feed,
            publish.pending_stories,
            publish.last_batch_capsules,
            plural(publish.last_batch_capsules)
        );
        if publish.last_edge_feeds > 0 || publish.last_edge_headlines > 0 {
            let _ = writeln!(
                output,
                "edge: {} accepted · {} feeds · {} headlines",
                publish.last_edge_accepted, publish.last_edge_feeds, publish.last_edge_headlines
            );
        }
        if publish.processor_sessions > 0
            || publish.processor_events_dropped > 0
            || publish.processor_sessions_skipped > 0
            || publish.ambiguous_internal_candidates > 0
        {
            let _ = writeln!(
                output,
                "processor guard: {} session{} · {} dropped · {} skipped · {} ambiguous",
                publish.processor_sessions,
                plural(publish.processor_sessions),
                publish.processor_events_dropped,
                publish.processor_sessions_skipped,
                publish.ambiguous_internal_candidates
            );
        }
        if let Some(detail) = &publish.detail {
            let _ = writeln!(output, "publish detail: {detail}");
        }
        if let Some(error) = &publish.last_error {
            let _ = writeln!(output, "publish error: {error}");
        }
    }

    if !status.capture_watchers.is_empty() {
        output.push_str("\nwatchers:\n");
        for watcher in status.capture_watchers.iter().take(8) {
            let append = watcher
                .last_append_ms
                .map(format_duration_ms)
                .unwrap_or_else(|| "unknown".to_string());
            let session = watcher.session_id.as_deref().unwrap_or("session");
            let _ = writeln!(
                output,
                "  {} {} {} · {} · last append {} · offset {} · {} imported · {} filtered",
                watcher.agent,
                watcher.adapter,
                watcher.state,
                session,
                append,
                watcher.offset,
                watcher.imported_events,
                watcher.filtered_events
            );
        }
        if status.capture_watchers.len() > 8 {
            let _ = writeln!(output, "  ... {} more", status.capture_watchers.len() - 8);
        }
    }

    if !status.captured_sources.is_empty() {
        output.push_str("\nsources:\n");
        for source in status.captured_sources.iter().take(8) {
            let _ = writeln!(
                output,
                "  {} {} · {} events · {} sessions · last {}",
                source.agent,
                source.adapter,
                source.events,
                source.sessions,
                source.last_event_kind
            );
        }
        if status.captured_sources.len() > 8 {
            let _ = writeln!(output, "  ... {} more", status.captured_sources.len() - 8);
        }
    }

    output.push('\n');
    output.push_str(&status_next_step(status));
    output.push('\n');
    output
}

fn status_next_step(status: &StatusView) -> String {
    if status.capture_watchers.is_empty() && status.captured_sources.is_empty() {
        "next: no capture watchers reported. start `agent-feed serve` without --no-agent-capture, or attach `agent-feed codex active --watch`.".to_string()
    } else if !status.capture_watchers.is_empty() && status.stored_events == 0 {
        "next: transcript watchers are live. continue or restart an agent session after feed is running so future writes can settle into stories.".to_string()
    } else if status.stored_events > 0 && status.stored_bulletins == 0 {
        if let Some(decision) = &status.story.last_decision {
            format!(
                "next: events are arriving, but the latest story gate is `{}`: {}.",
                decision.action, decision.reason
            )
        } else {
            "next: events are arriving. waiting for the story compiler to see completion, test, edit, or incident context worth publishing.".to_string()
        }
    } else if status.stored_bulletins > 0 {
        if let Some(publish) = &status.publish
            && matches!(publish.state.as_str(), "error" | "degraded")
        {
            return format!(
                "next: local stories are available, but publish is `{}`: {}.",
                publish.state,
                publish
                    .last_error
                    .as_deref()
                    .or(publish.detail.as_deref())
                    .unwrap_or("check p2p publish logs")
            );
        }
        "next: local stories are available. keep the browser open; it refreshes automatically."
            .to_string()
    } else {
        "next: feed is live.".to_string()
    }
}

fn story_status_is_active(story: &agent_feed_views::StoryStatusView) -> bool {
    story.open_windows > 0
        || story.retained_windows > 0
        || story.settled_windows > 0
        || story.published_stories > 0
        || story.rejected_stories > 0
        || story.deduped_stories > 0
        || story.last_decision.is_some()
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}s", ms / 1_000)
    } else {
        format!("{}m", ms / 60_000)
    }
}

fn get_json<T>(server: &str, path: &str) -> Result<T, CliError>
where
    T: DeserializeOwned,
{
    let response = get_http(server, path)?;
    let body = http_response_body(&response)?;
    Ok(serde_json::from_str(body.trim())?)
}

fn get_edge_json(edge: &str, path: &str) -> Result<Value, CliError> {
    let url = format!("{}{}", edge.trim_end_matches('/'), path);
    let output = ProcessCommand::new("curl")
        .args(["-fsS", "--connect-timeout", "5", "--max-time", "15", &url])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| CliError::Http(format!("failed to start curl: {err}")))?;
    if !output.status.success() {
        return Err(CliError::Http(format!(
            "edge request failed with {}: {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn get_http(server: &str, path: &str) -> Result<String, CliError> {
    let addr = server
        .parse()
        .map_err(|err| CliError::Http(format!("invalid server address: {err}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(700))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {server}\r\nAccept: application/json\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    if response.starts_with("HTTP/1.1 2") || response.starts_with("HTTP/1.0 2") {
        return Ok(response);
    }

    let status_line = response.lines().next().unwrap_or("http request failed");
    let body = http_response_body(&response).unwrap_or("").trim();
    if body.is_empty() {
        Err(CliError::Http(status_line.to_string()))
    } else {
        Err(CliError::Http(format!("{status_line}: {body}")))
    }
}

fn http_response_body(response: &str) -> Result<&str, CliError> {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or_else(|| CliError::Http("http response did not include a body".to_string()))
}

fn post_json(server: &str, path: &str, body: &str) -> Result<String, CliError> {
    let addr = server
        .parse()
        .map_err(|err| CliError::Http(format!("invalid server address: {err}")))?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(700))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(
        format!(
            "POST {path} HTTP/1.1\r\nHost: {server}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )?;
    stream.write_all(body.as_bytes())?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    if response.starts_with("HTTP/1.1 2") || response.starts_with("HTTP/1.0 2") {
        return Ok(response);
    }

    Err(CliError::Http(response))
}

fn load_publish_session(
    auth_store: Option<PathBuf>,
    edge: &str,
) -> Result<GithubAuthSession, CliError> {
    let store = GithubSessionStore::new(auth_store_path(auth_store));
    let Some(session) = store.load()? else {
        return Err(CliError::Http(
            "github auth required before live p2p publish; run `agent-feed auth github`"
                .to_string(),
        ));
    };
    if session.is_expired_at(time::OffsetDateTime::now_utc()) {
        return Err(CliError::Http(
            "github auth session expired; run `agent-feed auth github`".to_string(),
        ));
    }
    if session.session_token.as_deref().unwrap_or("").is_empty() {
        return Err(CliError::Http(
            "github auth session does not include a publish token; run `agent-feed auth github`"
                .to_string(),
        ));
    }
    let expected = edge.trim_end_matches('/');
    let actual = session.edge_base_url.trim_end_matches('/');
    if expected != actual {
        return Err(CliError::Http(format!(
            "github auth session is for {actual}; re-run auth with --edge {expected}"
        )));
    }
    Ok(session)
}

fn ensure_publish_session(
    auth_store: Option<PathBuf>,
    edge: &str,
    callback_bind: SocketAddr,
    timeout: Duration,
    no_browser: bool,
) -> Result<GithubAuthSession, CliError> {
    match load_publish_session(auth_store.clone(), edge) {
        Ok(session) => Ok(session),
        Err(err) => {
            warn!(
                %edge,
                error = %err,
                "github publish session unavailable; starting browser auth"
            );
            println!("github auth required for p2p publish; starting sign-in");
            github_cli_login(
                edge.to_string(),
                callback_bind,
                timeout,
                no_browser,
                no_browser,
                auth_store,
            )
        }
    }
}

fn github_cli_login(
    edge: String,
    callback_bind: SocketAddr,
    timeout: Duration,
    no_browser: bool,
    print_url: bool,
    store: Option<PathBuf>,
) -> Result<GithubAuthSession, CliError> {
    let listener = TcpListener::bind(callback_bind)?;
    let bind = listener.local_addr()?;
    if !bind.ip().is_loopback() {
        return Err(CliError::Http(
            "github auth callback must bind to loopback".to_string(),
        ));
    }
    info!(
        edge = %edge,
        callback_bind = %bind,
        timeout_secs = timeout.as_secs(),
        no_browser,
        print_url,
        "github cli auth starting"
    );
    let config = GithubCliAuthConfig {
        edge_base_url: edge.clone(),
        callback_bind,
        ..GithubCliAuthConfig::default()
    };
    let start = begin_cli_login(&config, bind)?;
    debug!(
        authorize_url = %start.authorize_url,
        callback_url = %start.callback_url,
        "github auth url prepared"
    );
    if print_url || no_browser {
        println!("{}", start.authorize_url);
    }
    if !no_browser && let Err(err) = open_url(&start.authorize_url) {
        warn!(error = %err, "failed to open browser for github auth");
        eprintln!("agent-feed: failed to open browser: {err}");
        eprintln!("agent-feed: open this URL manually:");
        eprintln!("{}", start.authorize_url);
    }
    println!("waiting for github callback on {}", start.callback_url);
    let (target, mut stream) = wait_for_github_callback(&listener, timeout)?;
    let session = match parse_cli_callback_request(&target)
        .and_then(|callback| complete_cli_login(&start, callback, edge.clone()))
    {
        Ok(session) => {
            write_auth_callback_response(&mut stream, true, "github sign-in complete")?;
            session
        }
        Err(err) => {
            let _ = write_auth_callback_response(&mut stream, false, "github sign-in failed");
            return Err(CliError::Auth(err));
        }
    };
    let store = GithubSessionStore::new(auth_store_path(store));
    store.save(&session)?;
    info!(
        github_login = %session.login,
        github_user_id = session.github_user_id,
        expires_at = %session.expires_at,
        "github cli auth completed"
    );
    Ok(session)
}

fn post_edge_json_with_bearer(
    edge: &str,
    path: &str,
    body: &str,
    session: &GithubAuthSession,
) -> Result<String, CliError> {
    let token = session
        .session_token
        .as_deref()
        .ok_or_else(|| CliError::Http("github session token missing".to_string()))?;
    let url = format!("{}{}", edge.trim_end_matches('/'), path);
    let header_file = write_private_temp(
        "agent-feed-header",
        format!("Authorization: Bearer {token}\n").as_bytes(),
    )?;
    let body_file = write_private_temp("agent-feed-body", body.as_bytes())?;
    let child = ProcessCommand::new("curl")
        .args([
            "-fsS",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-H",
            &format!("@{}", header_file.display()),
            "--data-binary",
            &format!("@{}", body_file.display()),
            &url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| CliError::Http(format!("failed to start curl: {err}")))?;
    let output = child
        .wait_with_output()
        .map_err(|err| CliError::Http(format!("curl failed: {err}")))?;
    let _ = fs::remove_file(&header_file);
    let _ = fs::remove_file(&body_file);
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(CliError::Http(format!(
        "edge publish failed with {}: {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )))
}

fn write_private_temp(prefix: &str, bytes: &[u8]) -> Result<PathBuf, CliError> {
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}.tmp",
        std::process::id(),
        time::OffsetDateTime::now_utc()
            .unix_timestamp_nanos()
            .unsigned_abs()
    ));
    fs::write(&path, bytes)?;
    set_owner_only_permissions(&path)?;
    Ok(path)
}

fn set_owner_only_permissions(path: &Path) -> Result<(), CliError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn auth_store_path(path: Option<PathBuf>) -> PathBuf {
    if let Some(path) = path {
        return path;
    }
    if let Ok(path) = std::env::var("AGENT_FEED_GITHUB_SESSION") {
        return PathBuf::from(path);
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agent_feed")
        .join("auth")
        .join("github.json")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn wait_for_github_callback(
    listener: &TcpListener,
    timeout: Duration,
) -> Result<(String, TcpStream), CliError> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let mut request = [0u8; 4096];
                let len = stream.read(&mut request)?;
                let request = String::from_utf8_lossy(&request[..len]);
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| {
                        let mut parts = line.split_whitespace();
                        match (parts.next(), parts.next()) {
                            (Some("GET"), Some(target)) => Some(target.to_string()),
                            _ => None,
                        }
                    })
                    .ok_or_else(|| CliError::Http("invalid github callback request".to_string()))?;
                return Ok((target, stream));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(CliError::Http(
                        "timed out waiting for github callback".to_string(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(CliError::Io(err)),
        }
    }
}

fn write_auth_callback_response(
    stream: &mut TcpStream,
    ok: bool,
    message: &str,
) -> Result<(), CliError> {
    let title = if ok {
        "agent_feed signed in"
    } else {
        "agent_feed sign-in failed"
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head><body><main style=\"font-family: ui-monospace, monospace; padding: 3rem;\"><h1>{}</h1><p>{}</p><p>you can return to the terminal.</p></main></body></html>",
        html_escape(title),
        html_escape(title),
        html_escape(message)
    );
    let status = if ok { "200 OK" } else { "400 Bad Request" };
    stream.write_all(
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .as_bytes(),
    )?;
    Ok(())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn open_url(url: &str) -> Result<(), CliError> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "cmd"
    } else {
        "xdg-open"
    };

    let status = if cfg!(target_os = "windows") {
        ProcessCommand::new(opener)
            .args(["/C", "start", "", url])
            .status()?
    } else {
        ProcessCommand::new(opener).arg(url).status()?
    };

    if status.success() {
        Ok(())
    } else {
        Err(CliError::Http(format!("{opener} exited with {status}")))
    }
}
