use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("invalid github login: {0}")]
    InvalidGithubLogin(String),
    #[error("invalid github org: {0}")]
    InvalidGithubOrg(String),
    #[error("invalid github team slug: {0}")]
    InvalidGithubTeamSlug(String),
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

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GithubOrgName(String);

impl GithubOrgName {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, IdentityError> {
        let value = value.as_ref().trim().trim_start_matches('@');
        if is_valid_github_org(value) {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(IdentityError::InvalidGithubOrg(value.to_string()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GithubOrgName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for GithubOrgName {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GithubTeamSlug(String);

impl GithubTeamSlug {
    pub fn parse(value: impl AsRef<str>) -> Result<Self, IdentityError> {
        let value = value.as_ref().trim();
        if is_valid_github_team_slug(value) {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(IdentityError::InvalidGithubTeamSlug(value.to_string()))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GithubTeamSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for GithubTeamSlug {
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
    GithubOrg {
        org: GithubOrgName,
    },
    GithubTeam {
        org: GithubOrgName,
        team: GithubTeamSlug,
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
            Self::GithubOrg { org } => format!("github-org:{org}"),
            Self::GithubTeam { org, team } => format!("github-team:{org}/{team}"),
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

#[must_use]
pub fn is_valid_github_org(value: &str) -> bool {
    is_valid_github_login(value)
}

#[must_use]
pub fn is_valid_github_team_slug(value: &str) -> bool {
    let len = value.len();
    len > 0
        && len <= 100
        && !value.starts_with('.')
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
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

    #[test]
    fn github_org_and_team_values_are_route_safe() {
        let org = GithubOrgName::parse("Aberration-Technology").expect("org parses");
        let team = GithubTeamSlug::parse("Release_Desk").expect("team parses");

        assert_eq!(org.as_str(), "aberration-technology");
        assert_eq!(team.as_str(), "release_desk");
        assert!(GithubOrgName::parse("../org").is_err());
        assert!(GithubTeamSlug::parse("../team").is_err());
    }
}
