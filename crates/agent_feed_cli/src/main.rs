use agent_feed_adapters::claude::{ClaudeState, normalize_stream_value};
use agent_feed_adapters::codex::{TranscriptState, normalize_transcript_value};
use agent_feed_auth_github::{
    GithubAuthError, GithubCliAuthConfig, GithubSessionStore, begin_cli_login, complete_cli_login,
    parse_cli_callback_request,
};
use agent_feed_core::AgentEvent;
use agent_feed_directory::RemoteUserRoute;
use agent_feed_edge::{EdgeConfig, EdgeServerConfig, OrgDeploymentPolicy, serve_http};
use agent_feed_ingest::source_from_str;
use agent_feed_install::{doctor_report, init_plan};
use agent_feed_p2p_proto::{FeedVisibility, Signed, StoryCapsule};
use agent_feed_security::SecurityConfig;
use agent_feed_server::{ServerConfig, serve};
use agent_feed_story::{CompiledStory, compile_events};
use agent_feed_summarize::{
    FeedSummaryMode, GuardrailPattern, ImageDecisionMode, ImageProcessorConfig, SummaryConfig,
    SummaryError, SummaryProcessorConfig, summarize_feed,
};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant};

const DEFAULT_URL: &str = "http://127.0.0.1:7777/reel";
const LOOPBACK_ADDR: &str = "127.0.0.1:7777";

#[derive(Debug, Parser)]
#[command(name = "agent-feed")]
#[command(about = "agent activity, reduced to signal")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
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
        #[arg(long, default_value = "https://edge.feed.aberration.technology")]
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
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 1000)]
        poll_ms: u64,
    },
    Import {
        paths: Vec<PathBuf>,
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
    },
}

#[derive(Debug, Subcommand)]
enum ClaudeCommand {
    Active {
        #[arg(long, default_value_t = 2)]
        sessions: usize,
        #[arg(long)]
        projects_dir: Option<PathBuf>,
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 1000)]
        poll_ms: u64,
    },
    Import {
        paths: Vec<PathBuf>,
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
    },
    Stream {
        #[arg(long, default_value = LOOPBACK_ADDR)]
        server: String,
    },
    Stories {
        #[arg(long, default_value_t = 2)]
        sessions: usize,
        #[arg(long)]
        projects_dir: Option<PathBuf>,
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
        #[arg(long, default_value = "workstation")]
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
        #[arg(long, default_value = "workstation")]
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
        #[arg(long, default_value = "deterministic")]
        summarizer: String,
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
        #[arg(long, default_value = "https://edge.feed.aberration.technology")]
        edge_base_url: String,
        #[arg(long, default_value = "https://feed.aberration.technology")]
        browser_app_base_url: String,
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
    tracing_subscriber::fmt()
        .with_env_filter("agent_feed=info,tower_http=warn")
        .init();

    let cli = Cli::parse();
    match cli.command {
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
            open_url(&url)?;
            println!("{url}");
        }
        Commands::Ingest { source, server } => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let path = endpoint_for_source(&source);
            for payload in payloads_from_input(&input)? {
                println!("{}", post_json(&server, path, &payload)?);
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
            let _ = post_json(&server, endpoint_for_source(&source), &payload);
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
                let config = GithubCliAuthConfig {
                    edge_base_url: edge.clone(),
                    callback_bind,
                    ..GithubCliAuthConfig::default()
                };
                let start = begin_cli_login(&config, bind)?;
                if print_url || no_browser {
                    println!("{}", start.authorize_url);
                }
                if !no_browser && let Err(err) = open_url(&start.authorize_url) {
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
                println!(
                    "github auth: @{} github:{} expires {}",
                    session.login, session.github_user_id, session.expires_at
                );
            }
            AuthCommand::Status { store } => {
                let store = GithubSessionStore::new(auth_store_path(store));
                match store.load()? {
                    Some(session) => {
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
                        println!("github auth: signed out");
                    }
                }
            }
            AuthCommand::Logout { store } => {
                let store = GithubSessionStore::new(auth_store_path(store));
                if store.delete()? {
                    println!("github auth: signed out");
                } else {
                    println!("github auth: already signed out");
                }
            }
        },
        Commands::Codex { command } => match command {
            CodexCommand::Active {
                sessions,
                history,
                sessions_dir,
                server,
                watch,
                poll_ms,
            } => {
                let history = history.unwrap_or_else(default_codex_history);
                let sessions_dir = sessions_dir.unwrap_or_else(default_codex_sessions_dir);
                let paths = active_codex_session_paths(&history, &sessions_dir, sessions)?;
                if watch {
                    watch_codex_sessions(&server, &paths, poll_ms)?;
                } else {
                    import_codex_sessions(&server, &paths)?;
                }
            }
            CodexCommand::Import { paths, server } => {
                import_codex_sessions(&server, &paths)?;
            }
            CodexCommand::Stories {
                sessions,
                history,
                sessions_dir,
            } => {
                let history = history.unwrap_or_else(default_codex_history);
                let sessions_dir = sessions_dir.unwrap_or_else(default_codex_sessions_dir);
                let paths = active_codex_session_paths(&history, &sessions_dir, sessions)?;
                let stories = compile_codex_stories(&paths)?;
                print_stories("codex", &paths, &stories)?;
            }
        },
        Commands::Claude { command } => match command {
            ClaudeCommand::Active {
                sessions,
                projects_dir,
                server,
                watch,
                poll_ms,
            } => {
                let projects_dir = projects_dir.unwrap_or_else(default_claude_projects_dir);
                let paths = active_claude_session_paths(&projects_dir, sessions)?;
                if watch {
                    watch_claude_sessions(&server, &paths, poll_ms)?;
                } else {
                    import_claude_sessions(&server, &paths)?;
                }
            }
            ClaudeCommand::Import { paths, server } => {
                import_claude_sessions(&server, &paths)?;
            }
            ClaudeCommand::Stream { server } => {
                let mut input = String::new();
                std::io::stdin().read_to_string(&mut input)?;
                let imported = import_claude_stream_input(&server, Path::new("<stdin>"), &input);
                println!("claude stream imported: {imported} events from stdin");
            }
            ClaudeCommand::Stories {
                sessions,
                projects_dir,
            } => {
                let projects_dir = projects_dir.unwrap_or_else(default_claude_projects_dir);
                let paths = active_claude_session_paths(&projects_dir, sessions)?;
                let stories = compile_claude_stories(&paths)?;
                print_stories("claude", &paths, &stories)?;
            }
        },
        Commands::P2p { command } => match command {
            P2pCommand::Init => {
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
                println!("p2p join staged: network={network} network_id={network_id}");
                for peer in bootstrap {
                    println!("bootstrap {peer}");
                }
            }
            P2pCommand::Status => {
                println!("p2p disabled; local reel is authoritative until agent-feed serve --p2p");
            }
            P2pCommand::Peers => {
                println!("no native p2p runtime is running in this process");
            }
            P2pCommand::Doctor => {
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
                println!("p2p share staged: feed={feed} visibility={visibility:?}");
                if let Some(org) = github_org {
                    println!("github_org={org}");
                }
                if let Some(team) = github_team {
                    println!("github_team={team}");
                }
                println!("raw_events=false summary_only=true encrypt_private_feeds=true");
            }
            P2pCommand::Pause => {
                println!("p2p publish paused");
            }
            P2pCommand::Resume => {
                println!("p2p publish resumed");
            }
            P2pCommand::Publish {
                dry_run,
                feed,
                sessions,
                agents,
                history,
                sessions_dir,
                claude_projects_dir,
                summarizer,
                per_story,
                allow_project_names,
                guardrail_pattern,
                images,
                image_processor,
                image_endpoint,
                image_command,
                image_args,
                allow_remote_image_urls,
            } => {
                if !dry_run {
                    return Err(CliError::Http(
                        "p2p publish requires --dry-run until a native runtime is configured"
                            .to_string(),
                    ));
                }
                let selected_agents = parse_agent_list(&agents);
                let mut captured_paths = Vec::new();
                let mut stories = Vec::new();
                if selected_agents.contains("codex") {
                    let history = history.unwrap_or_else(default_codex_history);
                    let sessions_dir = sessions_dir.unwrap_or_else(default_codex_sessions_dir);
                    let paths = active_codex_session_paths(&history, &sessions_dir, sessions)?;
                    stories.extend(compile_codex_stories(&paths)?);
                    captured_paths.extend(paths);
                }
                if selected_agents.contains("claude") {
                    let projects_dir =
                        claude_projects_dir.unwrap_or_else(default_claude_projects_dir);
                    let paths = active_claude_session_paths(&projects_dir, sessions)?;
                    stories.extend(compile_claude_stories(&paths)?);
                    captured_paths.extend(paths);
                }
                let summary_config = summary_config(SummaryCliOptions {
                    summarizer: &summarizer,
                    per_story,
                    allow_project_names,
                    guardrail_patterns: &guardrail_pattern,
                    images,
                    image_processor: &image_processor,
                    image_endpoint: image_endpoint.as_deref(),
                    image_command: image_command.as_deref(),
                    image_args: &image_args,
                    allow_remote_image_urls,
                })?;
                let capsules = signed_capsules(&feed, &stories, &summary_config)?;
                println!(
                    "p2p publish dry-run: {} summarized capsules from {} stories and {} local agent sessions",
                    capsules.len(),
                    stories.len(),
                    captured_paths.len()
                );
                for capsule in capsules.iter().take(8) {
                    println!("{}", serde_json::to_string(capsule)?);
                }
            }
        },
        Commands::Edge { command } => match command {
            EdgeCommand::Serve {
                bind,
                config: _,
                edge_base_url,
                browser_app_base_url,
                network_id,
            } => {
                let edge_host = edge_base_url
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_end_matches('/');
                let edge = EdgeConfig {
                    network_id,
                    edge_domain: edge_base_url.clone(),
                    browser_app_base_url,
                    bootstrap_peers: vec![
                        format!("/dns4/{edge_host}/tcp/7747"),
                        format!("/dns4/{edge_host}/udp/7747/quic-v1"),
                        format!("/dns4/{edge_host}/udp/443/webrtc-direct"),
                    ],
                    authority_id: "edge.feed".to_string(),
                    org_policy: OrgDeploymentPolicy::from_env(),
                };
                serve_http(EdgeServerConfig { bind, edge }).await?;
            }
            EdgeCommand::Health => {
                println!("feed edge: healthz=/healthz readyz=/readyz");
            }
        },
        Commands::Uninstall { restore_hooks } => {
            if restore_hooks {
                println!("restore-hooks requested; no installed hook manifest exists yet");
            } else {
                println!("no installed hook manifest exists yet");
            }
        }
    }
    Ok(())
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

fn import_codex_sessions(server: &str, paths: &[PathBuf]) -> Result<(), CliError> {
    if paths.is_empty() {
        println!("codex transcript import complete: 0 events; no sessions found");
        return Ok(());
    }

    let mut imported = 0usize;
    for path in paths {
        let input = fs::read_to_string(path)?;
        let mut state = TranscriptState::default();
        let file_events = import_codex_chunk(server, path, &input, &mut state);
        imported += file_events;
        println!(
            "codex transcript imported: {} events from {}",
            file_events,
            path.display()
        );
    }
    println!("codex transcript import complete: {imported} events");
    Ok(())
}

fn compile_codex_stories(paths: &[PathBuf]) -> Result<Vec<CompiledStory>, CliError> {
    let mut events = Vec::new();
    for path in paths {
        events.extend(collect_codex_events(path)?);
    }
    Ok(compile_events(events))
}

fn import_claude_sessions(server: &str, paths: &[PathBuf]) -> Result<(), CliError> {
    if paths.is_empty() {
        println!("claude stream import complete: 0 events; no sessions found");
        return Ok(());
    }

    let mut imported = 0usize;
    for path in paths {
        let input = fs::read_to_string(path)?;
        let file_events = import_claude_stream_input(server, path, &input);
        imported += file_events;
        println!(
            "claude stream imported: {} events from {}",
            file_events,
            path.display()
        );
    }
    println!("claude stream import complete: {imported} events");
    Ok(())
}

fn compile_claude_stories(paths: &[PathBuf]) -> Result<Vec<CompiledStory>, CliError> {
    let mut events = Vec::new();
    for path in paths {
        events.extend(collect_claude_events(path)?);
    }
    Ok(compile_events(events))
}

fn collect_codex_events(path: &Path) -> Result<Vec<AgentEvent>, CliError> {
    let input = fs::read_to_string(path)?;
    let mut state = TranscriptState::default();
    let mut events = Vec::new();
    for (index, line) in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(err) => {
                eprintln!(
                    "agent-feed: failed to parse codex transcript line {} in {}: {err}",
                    index + 1,
                    path.display()
                );
                continue;
            }
        };
        if let Some(event) = normalize_transcript_value(value, &mut state, Some(path)) {
            events.push(event);
        }
    }
    Ok(events)
}

fn collect_claude_events(path: &Path) -> Result<Vec<AgentEvent>, CliError> {
    let input = fs::read_to_string(path)?;
    let mut state = ClaudeState::default();
    let mut events = Vec::new();
    for (index, line) in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(err) => {
                eprintln!(
                    "agent-feed: failed to parse claude stream line {} in {}: {err}",
                    index + 1,
                    path.display()
                );
                continue;
            }
        };
        if let Some(event) = normalize_stream_value(value, &mut state, Some(path)) {
            events.push(event);
        }
    }
    Ok(events)
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
    for story in stories {
        println!("{}", serde_json::to_string(story)?);
    }
    Ok(())
}

fn signed_capsules(
    feed: &str,
    stories: &[CompiledStory],
    summary_config: &SummaryConfig,
) -> Result<Vec<Signed<StoryCapsule>>, CliError> {
    let feed_id = format!("local:{feed}");
    let summaries = summarize_feed(&feed_id, stories, summary_config)?;
    summaries
        .iter()
        .enumerate()
        .map(|(index, summary)| {
            let capsule = StoryCapsule::from_summary(
                feed_id.clone(),
                (index + 1) as u64,
                "local:codex",
                summary,
            )?;
            Signed::sign_capsule(capsule, "local-codex").map_err(CliError::from)
        })
        .collect()
}

struct SummaryCliOptions<'a> {
    summarizer: &'a str,
    per_story: bool,
    allow_project_names: bool,
    guardrail_patterns: &'a [String],
    images: bool,
    image_processor: &'a str,
    image_endpoint: Option<&'a str>,
    image_command: Option<&'a str>,
    image_args: &'a [String],
    allow_remote_image_urls: bool,
}

fn summary_config(options: SummaryCliOptions<'_>) -> Result<SummaryConfig, CliError> {
    let mut config = SummaryConfig::p2p_default();
    config.mode = if options.per_story {
        FeedSummaryMode::PerStory
    } else {
        FeedSummaryMode::FeedRollup
    };
    config.processor = match options.summarizer {
        "deterministic" => SummaryProcessorConfig::Deterministic,
        "codex" | "codex-exec" => SummaryProcessorConfig::CodexExec,
        "claude" | "claude-code" | "claude-code-exec" => SummaryProcessorConfig::ClaudeCodeExec,
        other => {
            return Err(CliError::Http(format!(
                "unknown summarizer {other}; use deterministic, codex-exec, or claude-code"
            )));
        }
    };
    config.guardrails.allow_project_names = options.allow_project_names;
    config.image.enabled = options.images;
    config.image.allow_remote_urls = options.allow_remote_image_urls;
    config.image.decision = ImageDecisionMode::BestJudgement;
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

fn watch_codex_sessions(server: &str, paths: &[PathBuf], poll_ms: u64) -> Result<(), CliError> {
    let mut watchers = Vec::new();
    for path in paths {
        let input = fs::read_to_string(path)?;
        let mut state = TranscriptState::default();
        let imported = import_codex_chunk(server, path, &input, &mut state);
        let offset = fs::metadata(path)?.len();
        println!(
            "codex transcript watching: {} ({} initial events)",
            path.display(),
            imported
        );
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
            let imported = import_codex_chunk(server, &watcher.path, &complete, &mut watcher.state);
            if imported > 0 {
                println!(
                    "codex transcript imported: {} appended events from {}",
                    imported,
                    watcher.path.display()
                );
            }
        }
    }
}

fn import_codex_chunk(
    server: &str,
    path: &Path,
    input: &str,
    state: &mut TranscriptState,
) -> usize {
    let mut imported = 0usize;
    for (index, line) in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(err) => {
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
        let body = match serde_json::to_string(&event) {
            Ok(body) => body,
            Err(err) => {
                eprintln!(
                    "agent-feed: failed to encode codex event from {}: {err}",
                    path.display()
                );
                continue;
            }
        };
        if let Err(err) = post_json(server, "/ingest/codex/jsonl", &body) {
            eprintln!(
                "agent-feed: failed to post codex event from {}: {err}",
                path.display()
            );
            continue;
        }
        imported += 1;
    }
    imported
}

fn watch_claude_sessions(server: &str, paths: &[PathBuf], poll_ms: u64) -> Result<(), CliError> {
    let mut watchers = Vec::new();
    for path in paths {
        let input = fs::read_to_string(path)?;
        let mut state = ClaudeState::default();
        let imported = import_claude_chunk(server, path, &input, &mut state);
        let offset = fs::metadata(path)?.len();
        println!(
            "claude stream watching: {} ({} initial events)",
            path.display(),
            imported
        );
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
            let imported =
                import_claude_chunk(server, &watcher.path, &complete, &mut watcher.state);
            if imported > 0 {
                println!(
                    "claude stream imported: {} appended events from {}",
                    imported,
                    watcher.path.display()
                );
            }
        }
    }
}

fn import_claude_stream_input(server: &str, path: &Path, input: &str) -> usize {
    let mut state = ClaudeState::default();
    import_claude_chunk(server, path, input, &mut state)
}

fn import_claude_chunk(server: &str, path: &Path, input: &str, state: &mut ClaudeState) -> usize {
    let mut imported = 0usize;
    for (index, line) in input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(err) => {
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
        let body = match serde_json::to_string(&event) {
            Ok(body) => body,
            Err(err) => {
                eprintln!(
                    "agent-feed: failed to encode claude event from {}: {err}",
                    path.display()
                );
                continue;
            }
        };
        if let Err(err) = post_json(server, "/ingest/claude/stream-json", &body) {
            eprintln!(
                "agent-feed: failed to post claude event from {}: {err}",
                path.display()
            );
            continue;
        }
        imported += 1;
    }
    imported
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
) -> Result<Vec<PathBuf>, CliError> {
    if limit == 0 || !history.exists() || !sessions_dir.exists() {
        return Ok(Vec::new());
    }

    let input = fs::read_to_string(history)?;
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for line in input.lines().rev() {
        let value = serde_json::from_str::<Value>(line)?;
        let Some(session_id) = value.get("session_id").and_then(Value::as_str) else {
            continue;
        };
        if seen.insert(session_id.to_string()) {
            ids.push(session_id.to_string());
        }
        if ids.len() >= limit {
            break;
        }
    }

    let mut paths = Vec::new();
    for session_id in ids {
        if let Some(path) = find_session_path(sessions_dir, &session_id)? {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn active_claude_session_paths(root: &Path, limit: usize) -> Result<Vec<PathBuf>, CliError> {
    if limit == 0 || !root.exists() {
        return Ok(Vec::new());
    }

    let mut paths = jsonl_files_by_mtime(root)?;
    paths.truncate(limit);
    Ok(paths)
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
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head><body><main style=\"font-family: ui-sans-serif, system-ui; padding: 3rem;\"><h1>{}</h1><p>{}</p><p>you can return to the terminal.</p></main></body></html>",
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
