use agent_reel_identity::{GithubLogin, GithubUserId, IdentityError, PrincipalRef};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum GithubResolveError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("github user not found: {0}")]
    NotFound(String),
    #[error("github resolver rate limited")]
    RateLimited,
    #[error("github resolver unavailable: {0}")]
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubProfile {
    pub id: GithubUserId,
    pub login: GithubLogin,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

impl GithubProfile {
    #[must_use]
    pub fn principal(&self) -> PrincipalRef {
        PrincipalRef::github(self.id, self.login.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct GithubUserResponse {
    pub id: u64,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

impl TryFrom<GithubUserResponse> for GithubProfile {
    type Error = GithubResolveError;

    fn try_from(value: GithubUserResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            id: GithubUserId::new(value.id),
            login: GithubLogin::parse(value.login)?,
            name: value.name,
            avatar_url: value.avatar_url,
        })
    }
}

pub trait GithubResolver {
    fn resolve_login(&self, login: &GithubLogin) -> Result<GithubProfile, GithubResolveError>;
}

#[derive(Clone, Debug, Default)]
pub struct StaticGithubResolver {
    profiles: BTreeMap<String, GithubProfile>,
    rate_limited: bool,
}

impl StaticGithubResolver {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_profile(mut self, profile: GithubProfile) -> Self {
        self.profiles.insert(profile.login.normalized(), profile);
        self
    }

    #[must_use]
    pub fn with_alias(mut self, alias: &GithubLogin, profile: GithubProfile) -> Self {
        self.profiles.insert(alias.normalized(), profile);
        self
    }

    #[must_use]
    pub fn rate_limited(mut self) -> Self {
        self.rate_limited = true;
        self
    }
}

impl GithubResolver for StaticGithubResolver {
    fn resolve_login(&self, login: &GithubLogin) -> Result<GithubProfile, GithubResolveError> {
        if self.rate_limited {
            return Err(GithubResolveError::RateLimited);
        }
        self.profiles
            .get(&login.normalized())
            .cloned()
            .ok_or_else(|| GithubResolveError::NotFound(login.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(login: &str, id: u64) -> GithubProfile {
        GithubProfile {
            id: GithubUserId::new(id),
            login: GithubLogin::parse(login).expect("login parses"),
            name: Some(login.to_string()),
            avatar_url: Some(format!("https://avatars.example/{id}.png")),
        }
    }

    #[test]
    fn resolver_maps_login_to_durable_user_id() {
        let resolver = StaticGithubResolver::new().with_profile(profile("mosure", 123));
        let resolved = resolver
            .resolve_login(&GithubLogin::parse("MoSuRe").expect("login parses"))
            .expect("profile resolves");
        assert_eq!(resolved.id, GithubUserId::new(123));
        assert_eq!(resolved.login.as_str(), "mosure");
    }

    #[test]
    fn renamed_login_alias_maps_to_same_id() {
        let old = GithubLogin::parse("old-login").expect("login parses");
        let resolver = StaticGithubResolver::new().with_alias(&old, profile("new-login", 123));
        let resolved = resolver.resolve_login(&old).expect("alias resolves");
        assert_eq!(resolved.id, GithubUserId::new(123));
        assert_eq!(resolved.login.as_str(), "new-login");
    }

    #[test]
    fn resolver_reports_not_found_and_rate_limit() {
        let login = GithubLogin::parse("unknown").expect("login parses");
        assert!(matches!(
            StaticGithubResolver::new().resolve_login(&login),
            Err(GithubResolveError::NotFound(_))
        ));
        assert!(matches!(
            StaticGithubResolver::new()
                .rate_limited()
                .resolve_login(&login),
            Err(GithubResolveError::RateLimited)
        ));
    }
}
