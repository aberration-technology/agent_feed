use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("invalid github login: {0}")]
    InvalidGithubLogin(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GithubLogin(String);

impl GithubLogin {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, IdentityError> {
        let value = value.as_ref().trim().trim_start_matches('@');
        if is_valid_github_login(value) {
            Ok(Self(value.to_string()))
        } else {
            Err(IdentityError::InvalidGithubLogin(value.to_string()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn normalized(&self) -> String {
        self.0.to_ascii_lowercase()
    }
}

impl fmt::Display for GithubLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for GithubLogin {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GithubUserId(u64);

impl GithubUserId {
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for GithubUserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrincipalRef {
    Github {
        id: GithubUserId,
        login: GithubLogin,
    },
    Local(String),
}

impl PrincipalRef {
    #[must_use]
    pub fn github(id: GithubUserId, login: GithubLogin) -> Self {
        Self::Github { id, login }
    }

    #[must_use]
    pub fn stable_key(&self) -> String {
        match self {
            Self::Github { id, .. } => format!("github:{}", id.get()),
            Self::Local(value) => format!("local:{value}"),
        }
    }
}

#[must_use]
pub fn is_valid_github_login(value: &str) -> bool {
    let len = value.len();
    if len == 0 || len > 39 {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
        return false;
    }
    bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_login_accepts_canonical_and_at_forms() {
        let login = GithubLogin::parse("@mosure").expect("login parses");
        assert_eq!(login.as_str(), "mosure");
        assert_eq!(login.normalized(), "mosure");
    }

    #[test]
    fn github_login_rejects_path_like_values() {
        assert!(GithubLogin::parse(".env").is_err());
        assert!(GithubLogin::parse("%2e%2e").is_err());
        assert!(GithubLogin::parse("foo/bar").is_err());
        assert!(GithubLogin::parse("-mosure").is_err());
    }
}
