use agent_feed_identity::{
    GithubLogin, GithubOrgName, GithubTeamSlug, GithubUserId, IdentityError, PrincipalRef,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, thiserror::Error)]
pub enum GithubResolveError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("github user not found: {0}")]
    NotFound(String),
    #[error("github resolver rate limited")]
    RateLimited,
    #[error("github user is not authorized for org policy: {0}")]
    Forbidden(String),
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubOrgProfile {
    pub login: GithubOrgName,
    pub id: Option<u64>,
    pub avatar_url: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubTeamProfile {
    pub org: GithubOrgName,
    pub slug: GithubTeamSlug,
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubOrgAccess {
    pub org: GithubOrgName,
    pub github_user_id: GithubUserId,
    pub teams: BTreeSet<GithubTeamSlug>,
}

impl GithubOrgAccess {
    #[must_use]
    pub fn new(
        org: GithubOrgName,
        github_user_id: GithubUserId,
        teams: impl IntoIterator<Item = GithubTeamSlug>,
    ) -> Self {
        Self {
            org,
            github_user_id,
            teams: teams.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn has_team(&self, team: &GithubTeamSlug) -> bool {
        self.teams.contains(team)
    }
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

pub trait GithubAccessResolver {
    fn org_access_for_user(
        &self,
        github_user_id: GithubUserId,
        org: &GithubOrgName,
    ) -> Result<GithubOrgAccess, GithubResolveError>;

    fn users_for_org(&self, org: &GithubOrgName) -> Result<Vec<GithubProfile>, GithubResolveError>;

    fn users_for_team(
        &self,
        org: &GithubOrgName,
        team: &GithubTeamSlug,
    ) -> Result<Vec<GithubProfile>, GithubResolveError>;
}

#[derive(Clone, Debug, Default)]
pub struct AllowAllGithubAccess;

impl GithubAccessResolver for AllowAllGithubAccess {
    fn org_access_for_user(
        &self,
        github_user_id: GithubUserId,
        org: &GithubOrgName,
    ) -> Result<GithubOrgAccess, GithubResolveError> {
        Ok(GithubOrgAccess::new(org.clone(), github_user_id, []))
    }

    fn users_for_org(
        &self,
        _org: &GithubOrgName,
    ) -> Result<Vec<GithubProfile>, GithubResolveError> {
        Ok(Vec::new())
    }

    fn users_for_team(
        &self,
        _org: &GithubOrgName,
        _team: &GithubTeamSlug,
    ) -> Result<Vec<GithubProfile>, GithubResolveError> {
        Ok(Vec::new())
    }
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

#[derive(Clone, Debug, Default)]
pub struct StaticGithubAccessResolver {
    members: BTreeMap<(String, GithubUserId), GithubOrgAccess>,
    profiles_by_org: BTreeMap<String, BTreeMap<GithubUserId, GithubProfile>>,
}

impl StaticGithubAccessResolver {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_member(
        mut self,
        org: GithubOrgName,
        profile: GithubProfile,
        teams: impl IntoIterator<Item = GithubTeamSlug>,
    ) -> Self {
        let access = GithubOrgAccess::new(org.clone(), profile.id, teams);
        self.profiles_by_org
            .entry(org.to_string())
            .or_default()
            .insert(profile.id, profile.clone());
        self.members.insert((org.to_string(), profile.id), access);
        self
    }
}

impl GithubAccessResolver for StaticGithubAccessResolver {
    fn org_access_for_user(
        &self,
        github_user_id: GithubUserId,
        org: &GithubOrgName,
    ) -> Result<GithubOrgAccess, GithubResolveError> {
        self.members
            .get(&(org.to_string(), github_user_id))
            .cloned()
            .ok_or_else(|| GithubResolveError::Forbidden(format!("github org {org}")))
    }

    fn users_for_org(&self, org: &GithubOrgName) -> Result<Vec<GithubProfile>, GithubResolveError> {
        Ok(self
            .profiles_by_org
            .get(&org.to_string())
            .map(|profiles| profiles.values().cloned().collect())
            .unwrap_or_default())
    }

    fn users_for_team(
        &self,
        org: &GithubOrgName,
        team: &GithubTeamSlug,
    ) -> Result<Vec<GithubProfile>, GithubResolveError> {
        let profiles = self
            .profiles_by_org
            .get(&org.to_string())
            .cloned()
            .unwrap_or_default();
        Ok(self
            .members
            .values()
            .filter(|access| access.org == *org && access.has_team(team))
            .filter_map(|access| profiles.get(&access.github_user_id).cloned())
            .collect())
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

    #[test]
    fn static_access_resolver_handles_org_and_team_membership() {
        let org = GithubOrgName::parse("aberration-technology").expect("org parses");
        let team = GithubTeamSlug::parse("release").expect("team parses");
        let profile = profile("mosure", 123);
        let resolver = StaticGithubAccessResolver::new().with_member(
            org.clone(),
            profile.clone(),
            [team.clone()],
        );

        let access = resolver
            .org_access_for_user(profile.id, &org)
            .expect("org access resolves");

        assert!(access.has_team(&team));
        assert_eq!(resolver.users_for_org(&org).expect("org users").len(), 1);
        assert_eq!(
            resolver.users_for_team(&org, &team).expect("team users")[0].id,
            GithubUserId::new(123)
        );
    }
}
