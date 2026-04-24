use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

type Result<T> = std::result::Result<T, XtaskError>;

#[derive(Debug)]
enum XtaskError {
    CommandFailed { command: String, status: ExitStatus },
    Io(std::io::Error),
    InvalidArgs(String),
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("doctor") => doctor(),
        Some("check") => check(args.collect()),
        Some("build-browser-site") => build_browser_site(args.collect()),
        Some("ui") => reserved_lane("ui"),
        Some("e2e") => reserved_lane("e2e"),
        Some("stress") => reserved_lane("stress"),
        Some(command) => {
            eprintln!("unknown xtask command: {command}");
            std::process::exit(2);
        }
        None => {
            eprintln!("usage: cargo xtask <doctor|check|build-browser-site|ui|e2e|stress>");
            std::process::exit(2);
        }
    }
}

fn doctor() -> Result<()> {
    run(cargo(), &["--version"])?;
    run(cargo(), &["metadata", "--no-deps", "--format-version", "1"])?;
    Ok(())
}

fn check(extra: Vec<String>) -> Result<()> {
    run(cargo_tool("cargo-fmt"), &["--all", "--", "--check"])?;
    run(
        cargo_tool("cargo-clippy"),
        &["--workspace", "--all-targets", "--", "-D", "warnings"],
    )?;
    run(cargo(), &["test", "--workspace"])?;
    if extra.iter().any(|arg| arg == "publish") {
        run(
            cargo(),
            &[
                "package",
                "--workspace",
                "--exclude",
                "xtask",
                "--allow-dirty",
                "--no-verify",
            ],
        )?;
    }
    Ok(())
}

fn reserved_lane(name: &str) -> Result<()> {
    println!("xtask {name} is not wired in this checkout");
    Ok(())
}

fn build_browser_site(args: Vec<String>) -> Result<()> {
    let mut out_dir = PathBuf::from("target/feed-site");
    let mut edge_url = "https://edge.feed.aberration.technology".to_string();
    let mut site_base_url = "https://feed.aberration.technology".to_string();
    let mut network_id = "agent-feed-mainnet".to_string();
    let mut cname = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--out-dir" => {
                out_dir = PathBuf::from(next_arg(&mut iter, "--out-dir")?);
            }
            "--edge-url" => {
                edge_url = next_arg(&mut iter, "--edge-url")?;
            }
            "--site-base-url" => {
                site_base_url = next_arg(&mut iter, "--site-base-url")?;
            }
            "--network-id" => {
                network_id = next_arg(&mut iter, "--network-id")?;
            }
            "--cname" => {
                let value = next_arg(&mut iter, "--cname")?;
                cname = (!value.trim().is_empty()).then_some(value);
            }
            "--no-cname" => {
                cname = None;
            }
            other => {
                return Err(XtaskError::InvalidArgs(format!(
                    "unknown build-browser-site argument: {other}"
                )));
            }
        }
    }

    fs::create_dir_all(&out_dir).map_err(XtaskError::Io)?;
    let index = render_browser_shell(&edge_url, &site_base_url, &network_id)?;
    fs::write(out_dir.join("index.html"), &index).map_err(XtaskError::Io)?;
    fs::write(out_dir.join("404.html"), &index).map_err(XtaskError::Io)?;
    fs::write(
        out_dir.join("feed-config.json"),
        format!(
            "{{\n  \"edge_base_url\": {},\n  \"site_base_url\": {},\n  \"network_id\": {}\n}}\n",
            js_string(&edge_url),
            js_string(&site_base_url),
            js_string(&network_id)
        ),
    )
    .map_err(XtaskError::Io)?;
    if let Some(cname) = cname {
        fs::write(out_dir.join("CNAME"), format!("{}\n", cname.trim())).map_err(XtaskError::Io)?;
    }
    println!("feed browser site written to {}", out_dir.display());
    Ok(())
}

fn next_arg(iter: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    iter.next()
        .ok_or_else(|| XtaskError::InvalidArgs(format!("{name} requires a value")))
}

fn render_browser_shell(edge_url: &str, site_base_url: &str, network_id: &str) -> Result<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| {
            XtaskError::InvalidArgs("xtask manifest path has no workspace root".into())
        })?;
    let ui_dir = root.join("crates/agent_feed_ui/src");
    let index = fs::read_to_string(ui_dir.join("index.html")).map_err(XtaskError::Io)?;
    let css = fs::read_to_string(ui_dir.join("reel.css")).map_err(XtaskError::Io)?;
    let js = fs::read_to_string(ui_dir.join("reel.ts")).map_err(XtaskError::Io)?;
    let config = format!(
        "window.FEED_P2P_ENABLED = true;\nwindow.FEED_EDGE_BASE_URL = {};\nwindow.FEED_SITE_BASE_URL = {};\nwindow.FEED_NETWORK_ID = {};\nwindow.AGENT_FEED_EDGE_BASE_URL = window.FEED_EDGE_BASE_URL;",
        js_string(edge_url),
        js_string(site_base_url),
        js_string(network_id),
    );
    Ok(index
        .replace("/*__REEL_CSS__*/", &css)
        .replace("/*__REEL_JS__*/", &js)
        .replace("/*__FEED_CONFIG__*/", "")
        .replace("__REEL_VIEW__", "remote")
        .replace(
            "<script type=\"module\">",
            &format!("<script>\n{config}\n    </script>\n    <script type=\"module\">"),
        ))
}

fn js_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => output.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn cargo() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

fn cargo_tool(tool: &str) -> String {
    let cargo = cargo();
    std::path::Path::new(&cargo)
        .parent()
        .map(|dir| dir.join(tool))
        .filter(|path| path.exists())
        .map_or_else(|| tool.to_string(), |path| path.display().to_string())
}

fn run(command: String, args: &[&str]) -> Result<()> {
    let status = Command::new(&command)
        .env("CARGO", cargo())
        .args(args)
        .status()
        .map_err(XtaskError::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(XtaskError::CommandFailed {
            command: format!("{command} {}", args.join(" ")),
            status,
        })
    }
}

impl std::fmt::Display for XtaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CommandFailed { command, status } => {
                write!(f, "{command} failed with {status}")
            }
            Self::Io(err) => write!(f, "{err}"),
            Self::InvalidArgs(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for XtaskError {}
