use agent_feed_adapters::claude::{ClaudeState, normalize_stream_value};
use agent_feed_adapters::codex::{TranscriptState, normalize_transcript_value};
use agent_feed_auth_github::{
    GithubAuthError, GithubAuthSession, GithubCliAuthConfig, GithubSessionStore, begin_cli_login,
    complete_cli_login, parse_cli_callback_request,
};
use agent_feed_core::AgentEvent;
use agent_feed_directory::{RemoteUserRoute, validate_logical_feed_label};
use agent_feed_edge::{
    EdgeConfig, EdgeFabricConfig, EdgeServerConfig, OrgDeploymentPolicy, serve_http,
};
use agent_feed_ingest::source_from_str;
use agent_feed_install::{doctor_report, init_plan};
use agent_feed_p2p_proto::{FeedVisibility, PublisherIdentity, Signed, StoryCapsule};
use agent_feed_security::SecurityConfig;
use agent_feed_server::{ServerConfig, serve};
use agent_feed_story::{CompiledStory, compile_events};
use agent_feed_summarize::{
    DEFAULT_SUMMARY_PROMPT_MAX_CHARS, DEFAULT_SUMMARY_PROMPT_STYLE, FeedSummaryMode,
    GuardrailPattern, ImageDecisionMode, ImageProcessorConfig, SummaryConfig, SummaryError,
    SummaryProcessorConfig, summarize_feed,
};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

const DEFAULT_URL: &str = "http://127.0.0.1:7777/reel";
const LOOPBACK_ADDR: &str = "127.0.0.1:7777";

#[derive(Debug, Parser)]
#[command(name = "agent-feed")]
#[command(about = "agent activity, reduced to signal")]
struct Cli {
    #[arg(
        long,
        global = true,
        visible_alias = "log-filter",
        default_value = "agent_feed=info,agent_feed_cli=info,tower_http=warn",
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

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Commands {
    Doctor,
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
    },
    Open {
        #[arg(default_value = DEFAULT_URL)]
        url: String,
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
        #[arg(long, default_value = "codex-exec")]
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
        Commands::Doctor => {
            let report = doctor_report();
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
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
            let p2p_enabled = p2p && !no_p2p;
            info!(
                %bind,
                p2p_enabled,
                p2p_requested = p2p,
                no_p2p,
                display_token_configured = security.display_token.is_some(),
                "serving local feed"
            );
            println!("serving http://{bind}/reel");
            if p2p_enabled {
                println!("p2p browser discovery ux enabled");
            }
            serve(ServerConfig {
                security,
                p2p_enabled,
            })
            .await?;
        }
        Commands::Open { url } => {
            info!(%url, "opening feed url");
            open_url(&url)?;
            println!("{url}");
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
                    timeout_secs,
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
                debug!(authorize_url = %start.authorize_url, callback_url = %start.callback_url, "github auth url prepared");
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
                let (target, mut stream) =
                    wait_for_github_callback(&listener, Duration::from_secs(timeout_secs))?;
                let session = match parse_cli_callback_request(&target)
                    .and_then(|callback| complete_cli_login(&start, callback, edge.clone()))
                {
                    Ok(session) => {
                        write_auth_callback_response(&mut stream, true, "github sign-in complete")?;
                        session
                    }
                    Err(err) => {
                        let _ = write_auth_callback_response(
                            &mut stream,
                            false,
                            "github sign-in failed",
                        );
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
                    watch_codex_sessions(&server, &paths, poll_ms, workspace_filter.as_ref())?;
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
                    watch_claude_sessions(&server, &paths, poll_ms, workspace_filter.as_ref())?;
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
                info!("p2p status requested");
                println!("p2p disabled; local reel is authoritative until agent-feed serve --p2p");
            }
            P2pCommand::Peers => {
                info!("p2p peers requested");
                println!("no native p2p runtime is running in this process");
            }
            P2pCommand::Doctor => {
                info!("p2p doctor requested");
                println!("p2p doctor: story capsule protocol ok; native transport not started");
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
                auth_store,
                feed,
                sessions,
                agents,
                history,
                sessions_dir,
                claude_projects_dir,
                workspace,
                summarizer,
                summary_style,
                summary_prompt_max_chars,
                per_story,
                allow_project_names,
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
                    agents = %agents,
                    selected_agents = ?selected_agents,
                    sessions,
                    workspace = workspace_filter.log_value(),
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
                    )?;
                    stories.extend(compile_codex_stories(&paths, workspace_filter.as_ref())?);
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
                    stories.extend(compile_claude_stories(&paths, workspace_filter.as_ref())?);
                    captured_paths.extend(paths);
                }
                let summary_config = summary_config(SummaryCliOptions {
                    summarizer: &summarizer,
                    summary_style: &summary_style,
                    summary_prompt_max_chars,
                    per_story,
                    allow_project_names,
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
                let session = if dry_run {
                    None
                } else {
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
                let capsules =
                    signed_capsules(&feed, &stories, &summary_config, publisher.as_ref())?;
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
                browser_app_base_url,
                github_callback_url,
                network_id,
            } => {
                let edge_host = edge_base_url
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_end_matches('/');
                info!(
                    %bind,
                    edge_base_url = %edge_base_url,
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
                        format!("/dns4/{edge_host}/tcp/7747"),
                        format!("/dns4/{edge_host}/udp/7747/quic-v1"),
                        format!("/dns4/{edge_host}/udp/443/webrtc-direct"),
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
            .with_ansi(ansi)
            .with_target(true)
            .with_env_filter(log_filter(filter))
            .init(),
        LogFormat::Pretty => tracing_subscriber::fmt()
            .pretty()
            .with_ansi(ansi)
            .with_target(true)
            .with_env_filter(log_filter(filter))
            .init(),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
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
                "agent_feed=info,agent_feed_cli=info,tower_http=warn",
            )
        })
}

fn command_name(command: &Commands) -> &'static str {
    match command {
        Commands::Doctor => "doctor",
        Commands::Init { .. } => "init",
        Commands::Serve { .. } => "serve",
        Commands::Open { .. } => "open",
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
    fn p2p_publish_defaults_to_aesthetic_headline_summarizer() {
        let cli = Cli::try_parse_from(["agent-feed", "p2p", "publish", "--dry-run"])
            .expect("publish command parses");

        let Commands::P2p {
            command:
                P2pCommand::Publish {
                    summarizer,
                    summary_style,
                    summary_prompt_max_chars,
                    ..
                },
        } = cli.command
        else {
            panic!("expected p2p publish command");
        };
        assert_eq!(summarizer, "codex-exec");
        assert_eq!(summary_style, DEFAULT_SUMMARY_PROMPT_STYLE);
        assert_eq!(summary_prompt_max_chars, DEFAULT_SUMMARY_PROMPT_MAX_CHARS);
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
    fn summary_config_rejects_unknown_routes_and_unsafe_image_config() {
        let unknown = summary_config(SummaryCliOptions {
            summarizer: "random-llm",
            summary_style: DEFAULT_SUMMARY_PROMPT_STYLE,
            summary_prompt_max_chars: DEFAULT_SUMMARY_PROMPT_MAX_CHARS,
            per_story: false,
            allow_project_names: false,
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
        let paths = active_codex_session_paths(&history, &sessions, 1, Some(&filter))
            .expect("active sessions resolve");

        assert_eq!(paths, vec![inside]);

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
        let mut state = TranscriptState::default();
        let file_stats = import_codex_chunk(server, path, &input, &mut state, workspace);
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

fn collect_codex_events(
    path: &Path,
    workspace: Option<&WorkspaceFilter>,
) -> Result<CollectedEvents, CliError> {
    let input = fs::read_to_string(path)?;
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
) -> Result<Vec<Signed<StoryCapsule>>, CliError> {
    let feed_id = format!("local:{feed}");
    let summaries = summarize_feed(&feed_id, stories, summary_config)?;
    info!(
        %feed_id,
        stories = stories.len(),
        summaries = summaries.len(),
        "feed stories summarized"
    );
    summaries
        .iter()
        .enumerate()
        .map(|(index, summary)| {
            let mut capsule = StoryCapsule::from_summary(
                feed_id.clone(),
                (index + 1) as u64,
                "local:codex",
                summary,
            )?;
            if let Some(publisher) = publisher {
                capsule = capsule.with_publisher(publisher.clone())?;
            }
            Signed::sign_capsule(capsule, "local-codex").map_err(CliError::from)
        })
        .collect()
}

struct SummaryCliOptions<'a> {
    summarizer: &'a str,
    summary_style: &'a str,
    summary_prompt_max_chars: usize,
    per_story: bool,
    allow_project_names: bool,
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
        "claude" | "claude-code" | "claude-code-exec" => SummaryProcessorConfig::ClaudeCodeExec,
        other => {
            return Err(CliError::Http(format!(
                "unknown summarizer {other}; use aesthetic, codex-exec, claude-code, or deterministic"
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

fn watch_codex_sessions(
    server: &str,
    paths: &[PathBuf],
    poll_ms: u64,
    workspace: Option<&WorkspaceFilter>,
) -> Result<(), CliError> {
    let mut watchers = Vec::new();
    for path in paths {
        let input = fs::read_to_string(path)?;
        let mut state = TranscriptState::default();
        let stats = import_codex_chunk(server, path, &input, &mut state, workspace);
        let offset = fs::metadata(path)?.len();
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
        watchers.push(CodexWatcher {
            path: path.clone(),
            offset,
            state,
            pending: String::new(),
        });
    }

    loop {
        std::thread::sleep(Duration::from_millis(poll_ms.max(100)));
        for watcher in &mut watchers {
            let len = fs::metadata(&watcher.path)?.len();
            if len <= watcher.offset {
                continue;
            }
            let mut file = File::open(&watcher.path)?;
            file.seek(SeekFrom::Start(watcher.offset))?;
            let mut chunk = String::new();
            file.read_to_string(&mut chunk)?;
            watcher.offset = len;
            watcher.pending.push_str(&chunk);
            let complete = split_complete_jsonl(&mut watcher.pending);
            let stats = import_codex_chunk(
                server,
                &watcher.path,
                &complete,
                &mut watcher.state,
                workspace,
            );
            if stats.imported > 0 || stats.filtered > 0 {
                info!(
                    path = %watcher.path.display(),
                    events = stats.imported,
                    filtered_events = stats.filtered,
                    offset = watcher.offset,
                    "codex transcript appended events imported"
                );
                print_watch_appended("codex transcript", &watcher.path, stats);
            }
        }
    }
}

fn import_codex_chunk(
    server: &str,
    path: &Path,
    input: &str,
    state: &mut TranscriptState,
    workspace: Option<&WorkspaceFilter>,
) -> ImportStats {
    let mut stats = ImportStats::default();
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
        stats.imported += 1;
    }
    stats
}

fn watch_claude_sessions(
    server: &str,
    paths: &[PathBuf],
    poll_ms: u64,
    workspace: Option<&WorkspaceFilter>,
) -> Result<(), CliError> {
    let mut watchers = Vec::new();
    for path in paths {
        let input = fs::read_to_string(path)?;
        let mut state = ClaudeState::default();
        let stats = import_claude_chunk(server, path, &input, &mut state, workspace);
        let offset = fs::metadata(path)?.len();
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
        watchers.push(ClaudeWatcher {
            path: path.clone(),
            offset,
            state,
            pending: String::new(),
        });
    }

    loop {
        std::thread::sleep(Duration::from_millis(poll_ms.max(100)));
        for watcher in &mut watchers {
            let len = fs::metadata(&watcher.path)?.len();
            if len <= watcher.offset {
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
            );
            if stats.imported > 0 || stats.filtered > 0 {
                info!(
                    path = %watcher.path.display(),
                    events = stats.imported,
                    filtered_events = stats.filtered,
                    offset = watcher.offset,
                    "claude stream appended events imported"
                );
                print_watch_appended("claude stream", &watcher.path, stats);
            }
        }
    }
}

fn import_claude_stream_input(
    server: &str,
    path: &Path,
    input: &str,
    workspace: Option<&WorkspaceFilter>,
) -> ImportStats {
    let mut state = ClaudeState::default();
    import_claude_chunk(server, path, input, &mut state, workspace)
}

fn import_claude_chunk(
    server: &str,
    path: &Path,
    input: &str,
    state: &mut ClaudeState,
    workspace: Option<&WorkspaceFilter>,
) -> ImportStats {
    let mut stats = ImportStats::default();
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
        stats.imported += 1;
    }
    stats
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

#[derive(Debug)]
struct CodexWatcher {
    path: PathBuf,
    offset: u64,
    state: TranscriptState,
    pending: String,
}

#[derive(Debug)]
struct ClaudeWatcher {
    path: PathBuf,
    offset: u64,
    state: ClaudeState,
    pending: String,
}

fn active_codex_session_paths(
    history: &Path,
    sessions_dir: &Path,
    limit: usize,
    workspace: Option<&WorkspaceFilter>,
) -> Result<Vec<PathBuf>, CliError> {
    if limit == 0 || !history.exists() || !sessions_dir.exists() {
        return Ok(Vec::new());
    }

    let input = fs::read_to_string(history)?;
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for line in input.lines().rev() {
        let value = serde_json::from_str::<Value>(line)?;
        let Some(session_id) = value.get("session_id").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(session_id.to_string()) {
            continue;
        }
        if let Some(path) = find_session_path(sessions_dir, session_id)? {
            if session_matches_workspace(&path, workspace, collect_codex_events)? {
                paths.push(path);
            } else if let Some(workspace) = workspace {
                debug!(
                    session_id,
                    path = %path.display(),
                    workspace = %workspace.display(),
                    "codex active session skipped by workspace filter"
                );
            }
        }
        if paths.len() >= limit {
            break;
        }
    }
    Ok(paths)
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
    let mut child = ProcessCommand::new("curl")
        .args([
            "-fsS",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-H",
            &format!("Authorization: Bearer {token}"),
            "--data-binary",
            "@-",
            &url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| CliError::Http(format!("failed to start curl: {err}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(body.as_bytes())?;
    }
    let output = child
        .wait_with_output()
        .map_err(|err| CliError::Http(format!("curl failed: {err}")))?;
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
