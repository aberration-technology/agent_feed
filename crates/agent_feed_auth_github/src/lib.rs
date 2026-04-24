use agent_feed_auth::Principal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use time::{Duration, OffsetDateTime};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubProfile {
    pub id: String,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

impl GithubProfile {
    #[must_use]
    pub fn principal(&self) -> Principal {
        Principal::github(self.id.clone(), self.login.clone())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GithubAuthError {
    #[error("github auth callback path was not /callback/github")]
    InvalidCallbackPath,
    #[error("github auth callback state mismatch")]
    StateMismatch,
    #[error("github auth callback was missing {0}")]
    MissingCallbackField(&'static str),
    #[error("github auth callback contained invalid github_user_id: {0}")]
    InvalidGithubUserId(String),
    #[error("github auth callback contained invalid expires_at: {0}")]
    InvalidExpiresAt(String),
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("json failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubCliAuthConfig {
    pub edge_base_url: String,
    pub callback_bind: SocketAddr,
    pub callback_path: String,
    pub requested_scopes: Vec<String>,
}

impl Default for GithubCliAuthConfig {
    fn default() -> Self {
        Self {
            edge_base_url: "https://edge.feed.aberration.technology".to_string(),
            callback_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            callback_path: "/callback/github".to_string(),
            requested_scopes: vec!["read:user".to_string()],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubCliLoginStart {
    pub authorize_url: String,
    pub callback_url: String,
    pub state: String,
    pub bind: SocketAddr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubCliCallback {
    pub state: String,
    pub github_user_id: u64,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub session_token: Option<String>,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubAuthSession {
    pub provider: String,
    pub github_user_id: u64,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub session_token: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub edge_base_url: String,
}

impl GithubAuthSession {
    #[must_use]
    pub fn profile(&self) -> GithubProfile {
        GithubProfile {
            id: self.github_user_id.to_string(),
            login: self.login.clone(),
            name: self.name.clone(),
            avatar_url: self.avatar_url.clone(),
        }
    }

    #[must_use]
    pub fn is_expired_at(&self, now: OffsetDateTime) -> bool {
        self.expires_at <= now
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubSessionStore {
    pub path: PathBuf,
}

impl GithubSessionStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<Option<GithubAuthSession>, GithubAuthError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let input = fs::read_to_string(&self.path)?;
        Ok(Some(serde_json::from_str(&input)?))
    }

    pub fn save(&self, session: &GithubAuthSession) -> Result<(), GithubAuthError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(session)?)?;
        set_owner_only_permissions(&tmp)?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }

    pub fn delete(&self) -> Result<bool, GithubAuthError> {
        if self.path.exists() {
            fs::remove_file(&self.path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub fn begin_cli_login(
    config: &GithubCliAuthConfig,
    bind: SocketAddr,
) -> Result<GithubCliLoginStart, GithubAuthError> {
    begin_cli_login_with_state(config, bind, &generate_state()?)
}

pub fn begin_cli_login_with_state(
    config: &GithubCliAuthConfig,
    bind: SocketAddr,
    state: &str,
) -> Result<GithubCliLoginStart, GithubAuthError> {
    let callback_url = format!(
        "http://{}{}",
        bind,
        normalize_callback_path(&config.callback_path)
    );
    let mut query = vec![
        ("client", "feed-cli".to_string()),
        ("state", state.to_string()),
        ("redirect_uri", callback_url.clone()),
    ];
    if !config.requested_scopes.is_empty() {
        query.push(("scope", config.requested_scopes.join(" ")));
    }
    let authorize_url = format!(
        "{}/auth/github?{}",
        config.edge_base_url.trim_end_matches('/'),
        encode_query(&query)
    );
    Ok(GithubCliLoginStart {
        authorize_url,
        callback_url,
        state: state.to_string(),
        bind,
    })
}

pub fn complete_cli_login(
    start: &GithubCliLoginStart,
    callback: GithubCliCallback,
    edge_base_url: impl Into<String>,
) -> Result<GithubAuthSession, GithubAuthError> {
    if callback.state != start.state {
        return Err(GithubAuthError::StateMismatch);
    }
    Ok(GithubAuthSession {
        provider: "github".to_string(),
        github_user_id: callback.github_user_id,
        login: callback.login,
        name: callback.name,
        avatar_url: callback.avatar_url,
        session_token: callback.session_token,
        issued_at: OffsetDateTime::now_utc(),
        expires_at: callback.expires_at,
        edge_base_url: edge_base_url.into(),
    })
}

pub fn parse_cli_callback_request(
    request_target: &str,
) -> Result<GithubCliCallback, GithubAuthError> {
    let (path, query) = request_target
        .split_once('?')
        .unwrap_or((request_target, ""));
    if path != "/callback/github" {
        return Err(GithubAuthError::InvalidCallbackPath);
    }
    let params = parse_query(query);
    let state = required(&params, "state")?.to_string();
    let id = required(&params, "github_user_id")
        .or_else(|_| required(&params, "id"))?
        .parse::<u64>()
        .map_err(|err| GithubAuthError::InvalidGithubUserId(err.to_string()))?;
    let login = required(&params, "login")?.to_string();
    let expires_at = params
        .get("expires_at")
        .map(|value| {
            OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                .map_err(|err| GithubAuthError::InvalidExpiresAt(err.to_string()))
        })
        .transpose()?
        .unwrap_or_else(|| OffsetDateTime::now_utc() + Duration::days(7));

    Ok(GithubCliCallback {
        state,
        github_user_id: id,
        login,
        name: params
            .get("name")
            .cloned()
            .filter(|value| !value.is_empty()),
        avatar_url: params
            .get("avatar_url")
            .or_else(|| params.get("avatar"))
            .cloned()
            .filter(|value| !value.is_empty()),
        session_token: params
            .get("session")
            .or_else(|| params.get("session_token"))
            .or_else(|| params.get("grant"))
            .cloned()
            .filter(|value| !value.is_empty()),
        expires_at,
    })
}

pub fn browser_sign_in_url(edge_base_url: &str, return_to: &str) -> String {
    format!(
        "{}/auth/github?{}",
        edge_base_url.trim_end_matches('/'),
        encode_query(&[
            ("client", "feed-browser".to_string()),
            ("return_to", return_to.to_string()),
        ])
    )
}

fn required<'a>(
    params: &'a BTreeMap<String, String>,
    key: &'static str,
) -> Result<&'a str, GithubAuthError> {
    params
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(GithubAuthError::MissingCallbackField(key))
}

fn normalize_callback_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn generate_state() -> Result<String, GithubAuthError> {
    let mut bytes = [0u8; 24];
    match fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes)) {
        Ok(()) => Ok(hex(&bytes)),
        Err(_) => {
            let now = OffsetDateTime::now_utc().unix_timestamp_nanos();
            let pid = std::process::id();
            Ok(hex(format!("{now}:{pid}").as_bytes()))
        }
    }
}

fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", url_encode(key), url_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            Some((url_decode(key)?, url_decode(value)?))
        })
        .collect()
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![byte as char]
            }
            b' ' => vec!['%', '2', '0'],
            other => format!("%{other:02X}").chars().collect(),
        })
        .collect()
}

fn url_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hi = chars.next()?;
            let lo = chars.next()?;
            let hex = [hi, lo];
            let text = std::str::from_utf8(&hex).ok()?;
            bytes.push(u8::from_str_radix(text, 16).ok()?);
        } else if byte == b'+' {
            bytes.push(b' ');
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn set_owner_only_permissions(path: &Path) -> Result<(), GithubAuthError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_login_start_uses_loopback_redirect_and_state() {
        let config = GithubCliAuthConfig {
            edge_base_url: "https://edge.example".to_string(),
            ..GithubCliAuthConfig::default()
        };
        let start = begin_cli_login_with_state(
            &config,
            SocketAddr::from(([127, 0, 0, 1], 49152)),
            "state one",
        )
        .expect("login starts");

        assert!(
            start
                .authorize_url
                .starts_with("https://edge.example/auth/github?")
        );
        assert!(start.authorize_url.contains("client=feed-cli"));
        assert!(start.authorize_url.contains("state=state%20one"));
        assert!(
            start
                .authorize_url
                .contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A49152%2Fcallback%2Fgithub")
        );
        assert_eq!(start.callback_url, "http://127.0.0.1:49152/callback/github");
    }

    #[test]
    fn callback_parses_profile_and_session() {
        let callback = parse_cli_callback_request(
            "/callback/github?state=s1&github_user_id=123&login=mosure&name=mosure&avatar_url=%2Favatar%2Fgithub%2F123&session=grant",
        )
        .expect("callback parses");

        assert_eq!(callback.state, "s1");
        assert_eq!(callback.github_user_id, 123);
        assert_eq!(callback.login, "mosure");
        assert_eq!(callback.avatar_url.as_deref(), Some("/avatar/github/123"));
        assert_eq!(callback.session_token.as_deref(), Some("grant"));
    }

    #[test]
    fn complete_login_rejects_state_mismatch() {
        let start = GithubCliLoginStart {
            authorize_url: String::new(),
            callback_url: String::new(),
            state: "expected".to_string(),
            bind: SocketAddr::from(([127, 0, 0, 1], 49152)),
        };
        let callback = GithubCliCallback {
            state: "other".to_string(),
            github_user_id: 123,
            login: "mosure".to_string(),
            name: None,
            avatar_url: None,
            session_token: None,
            expires_at: OffsetDateTime::now_utc() + Duration::hours(1),
        };

        assert!(matches!(
            complete_cli_login(&start, callback, "https://edge.example"),
            Err(GithubAuthError::StateMismatch)
        ));
    }

    #[test]
    fn browser_sign_in_targets_edge_auth() {
        let url = browser_sign_in_url("https://edge.example/", "https://app.example/mosure?all");
        assert!(url.starts_with("https://edge.example/auth/github?"));
        assert!(url.contains("client=feed-browser"));
        assert!(url.contains("return_to=https%3A%2F%2Fapp.example%2Fmosure%3Fall"));
    }
}
