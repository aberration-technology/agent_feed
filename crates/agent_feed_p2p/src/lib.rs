use agent_feed_directory::{
    DirectoryError, FeedAccessPolicy, FeedDirectoryEntry, ensure_current_compatibility,
};
use agent_feed_identity::{GithubOrgName, GithubTeamSlug, GithubUserId};
use agent_feed_p2p_proto::{
    FeedId, FeedProfile, FeedVisibility, NetworkId, PeerIdString, ProtocolCompatibility, Signed,
    StoryCapsule, feed_topic,
};
use agent_feed_summarize::headline_similarity;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PeerRole {
    Fabric,
    Publisher,
    Subscriber,
    Bootstrap,
    Relay,
    BrowserHandoff,
    Rendezvous,
    KadProvider,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerParticipation {
    pub network_id: NetworkId,
    pub peer_id: PeerIdString,
    pub compatibility: ProtocolCompatibility,
    pub principal: String,
    pub roles: BTreeSet<PeerRole>,
    pub browser_handoff_addrs: Vec<String>,
}

impl PeerParticipation {
    fn new(peer: &PeerNode) -> Self {
        Self {
            network_id: peer.network_id.clone(),
            peer_id: peer.peer_id.clone(),
            compatibility: peer.compatibility.clone(),
            principal: peer.principal.clone(),
            roles: BTreeSet::new(),
            browser_handoff_addrs: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_subscriber(&self) -> bool {
        self.roles.contains(&PeerRole::Subscriber)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum P2pError {
    #[error("p2p state lock poisoned")]
    StatePoisoned,
    #[error("feed not found: {0}")]
    FeedNotFound(String),
    #[error("subscription denied for feed: {0}")]
    SubscriptionDenied(String),
    #[error("capsule signature rejected")]
    InvalidSignature,
    #[error("p2p compatibility rejected: {0}")]
    IncompatibleProtocol(String),
    #[error(transparent)]
    Proto(#[from] agent_feed_p2p_proto::ProtoError),
    #[error(transparent)]
    Directory(#[from] DirectoryError),
}

#[derive(Clone, Debug)]
pub struct InMemoryNetwork {
    state: Arc<Mutex<NetworkState>>,
}

impl Default for InMemoryNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryNetwork {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(NetworkState::default())),
        }
    }

    pub fn peer(
        &self,
        network_id: impl Into<String>,
        peer_id: impl Into<String>,
        principal: impl Into<String>,
    ) -> PeerNode {
        PeerNode {
            network_id: network_id.into(),
            peer_id: peer_id.into(),
            compatibility: ProtocolCompatibility::current(),
            principal: principal.into(),
            network: self.clone(),
        }
    }

    pub fn peer_with_compatibility(
        &self,
        network_id: impl Into<String>,
        peer_id: impl Into<String>,
        principal: impl Into<String>,
        compatibility: ProtocolCompatibility,
    ) -> PeerNode {
        PeerNode {
            network_id: network_id.into(),
            peer_id: peer_id.into(),
            compatibility,
            principal: principal.into(),
            network: self.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PeerNode {
    pub network_id: NetworkId,
    pub peer_id: PeerIdString,
    pub compatibility: ProtocolCompatibility,
    pub principal: String,
    network: InMemoryNetwork,
}

impl PeerNode {
    pub fn join_fabric<I>(&self, roles: I) -> Result<(), P2pError>
    where
        I: IntoIterator<Item = PeerRole>,
    {
        self.ensure_compatible()?;
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let participation = state
            .participants
            .entry(self.peer_id.clone())
            .or_insert_with(|| PeerParticipation::new(self));
        participation.roles.insert(PeerRole::Fabric);
        participation.roles.extend(roles);
        let roles = participation.roles.clone();
        state
            .inboxes
            .entry(self.peer_id.clone())
            .or_insert_with(VecDeque::new);
        tracing::info!(
            network_id = %self.network_id,
            peer_id = %self.peer_id,
            principal = %self.principal,
            protocol_version = self.compatibility.protocol_version,
            model_version = self.compatibility.model_version,
            roles = ?roles,
            "p2p peer joined fabric"
        );
        Ok(())
    }

    pub fn register_browser_handoff<I>(&self, addrs: I) -> Result<(), P2pError>
    where
        I: IntoIterator<Item = String>,
    {
        self.ensure_compatible()?;
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let participation = state
            .participants
            .entry(self.peer_id.clone())
            .or_insert_with(|| PeerParticipation::new(self));
        participation.roles.insert(PeerRole::Fabric);
        participation.roles.insert(PeerRole::BrowserHandoff);
        participation.browser_handoff_addrs = addrs.into_iter().collect();
        let addrs_len = participation.browser_handoff_addrs.len();
        state
            .inboxes
            .entry(self.peer_id.clone())
            .or_insert_with(VecDeque::new);
        tracing::info!(
            network_id = %self.network_id,
            peer_id = %self.peer_id,
            browser_handoff_addrs = addrs_len,
            "p2p browser handoff registered"
        );
        Ok(())
    }

    pub fn announce_feed(&self, profile: FeedProfile) -> Result<(), P2pError> {
        self.ensure_compatible()?;
        ensure_current_compatibility(&profile.compatibility)?;
        let feed_id = profile.feed_id.clone();
        let network_id = profile.network_id.clone();
        let visibility = profile.visibility;
        let protocol_version = profile.compatibility.protocol_version;
        let model_version = profile.compatibility.model_version;
        let topic = feed_topic(&profile.network_id, &profile.feed_id);
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        state.topics.insert(profile.feed_id.clone(), topic.clone());
        state.feeds.insert(profile.feed_id.clone(), profile);
        state
            .participants
            .entry(self.peer_id.clone())
            .or_insert_with(|| PeerParticipation::new(self))
            .roles
            .insert(PeerRole::Publisher);
        state
            .inboxes
            .entry(self.peer_id.clone())
            .or_insert_with(VecDeque::new);
        tracing::info!(
            network_id = %network_id,
            peer_id = %self.peer_id,
            %feed_id,
            topic = %topic,
            visibility = ?visibility,
            protocol_version,
            model_version,
            "p2p feed announced"
        );
        Ok(())
    }

    pub fn announce_directory_entry(&self, entry: FeedDirectoryEntry) -> Result<(), P2pError> {
        self.ensure_compatible()?;
        ensure_current_compatibility(&entry.compatibility)?;
        if !entry.verify_signature()? {
            return Err(P2pError::Directory(DirectoryError::InvalidSignature));
        }
        if !entry.access_matches_visibility() {
            return Err(P2pError::Directory(DirectoryError::AccessPolicyMismatch));
        }
        if entry.peer_id != self.peer_id {
            tracing::warn!(
                peer_id = %self.peer_id,
                entry_peer_id = %entry.peer_id,
                feed_id = %entry.feed_id,
                "p2p directory announce rejected for mismatched peer"
            );
            return Err(P2pError::SubscriptionDenied(entry.feed_id));
        }
        let feed_id = entry.feed_id.clone();
        let github_user_id = entry.owner.github_user_id;
        let feed_label = entry.feed_label.clone();
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        state.index_directory_entry(entry);
        state
            .participants
            .entry(self.peer_id.clone())
            .or_insert_with(|| PeerParticipation::new(self))
            .roles
            .insert(PeerRole::KadProvider);
        tracing::info!(
            peer_id = %self.peer_id,
            %feed_id,
            %feed_label,
            github_user_id = ?github_user_id,
            "p2p directory entry announced"
        );
        Ok(())
    }

    pub fn cache_directory_entry(&self, entry: FeedDirectoryEntry) -> Result<(), P2pError> {
        self.ensure_compatible()?;
        ensure_current_compatibility(&entry.compatibility)?;
        if !entry.verify_signature()? {
            return Err(P2pError::Directory(DirectoryError::InvalidSignature));
        }
        if !entry.access_matches_visibility() {
            return Err(P2pError::Directory(DirectoryError::AccessPolicyMismatch));
        }
        let feed_id = entry.feed_id.clone();
        let github_user_id = entry.owner.github_user_id;
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        state.index_directory_entry(entry);
        let participation = state
            .participants
            .entry(self.peer_id.clone())
            .or_insert_with(|| PeerParticipation::new(self));
        participation.roles.insert(PeerRole::Fabric);
        participation.roles.insert(PeerRole::KadProvider);
        state
            .inboxes
            .entry(self.peer_id.clone())
            .or_insert_with(VecDeque::new);
        tracing::debug!(
            peer_id = %self.peer_id,
            %feed_id,
            github_user_id = ?github_user_id,
            "p2p directory entry cached"
        );
        Ok(())
    }

    pub fn certify_github_org_access<I>(&self, org: GithubOrgName, teams: I) -> Result<(), P2pError>
    where
        I: IntoIterator<Item = GithubTeamSlug>,
    {
        self.ensure_compatible()?;
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        state.org_access.insert(
            self.peer_id.clone(),
            PeerOrgAccess {
                org,
                teams: teams.into_iter().collect(),
            },
        );
        if let Some(access) = state.org_access.get(&self.peer_id) {
            tracing::info!(
                peer_id = %self.peer_id,
                org = %access.org,
                teams = ?access.teams,
                "p2p github org access certified"
            );
        }
        Ok(())
    }

    pub fn discover_github_user(
        &self,
        github_user_id: GithubUserId,
    ) -> Result<Vec<FeedDirectoryEntry>, P2pError> {
        self.ensure_compatible()?;
        let state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let entries: Vec<_> = state
            .directory
            .get(&github_user_id)
            .map(|entries| entries.values().cloned().collect())
            .unwrap_or_default();
        tracing::debug!(
            peer_id = %self.peer_id,
            github_user_id = ?github_user_id,
            feeds = entries.len(),
            "p2p github user discovery completed"
        );
        Ok(entries)
    }

    pub fn discover_github_org(
        &self,
        org: &GithubOrgName,
    ) -> Result<Vec<FeedDirectoryEntry>, P2pError> {
        self.ensure_compatible()?;
        let state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let entries: Vec<_> = state
            .org_directory
            .get(org)
            .map(|entries| entries.values().cloned().collect())
            .unwrap_or_default();
        tracing::debug!(
            peer_id = %self.peer_id,
            org = %org,
            feeds = entries.len(),
            "p2p github org discovery completed"
        );
        Ok(entries)
    }

    pub fn discover_github_team(
        &self,
        org: &GithubOrgName,
        team: &GithubTeamSlug,
    ) -> Result<Vec<FeedDirectoryEntry>, P2pError> {
        self.ensure_compatible()?;
        let state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let entries: Vec<_> = state
            .team_directory
            .get(&(org.clone(), team.clone()))
            .map(|entries| entries.values().cloned().collect())
            .unwrap_or_default();
        tracing::debug!(
            peer_id = %self.peer_id,
            org = %org,
            team = %team,
            feeds = entries.len(),
            "p2p github team discovery completed"
        );
        Ok(entries)
    }

    pub fn follow(&self, feed_id: &str) -> Result<(), P2pError> {
        self.ensure_compatible()?;
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let profile = state
            .feeds
            .get(feed_id)
            .ok_or_else(|| P2pError::FeedNotFound(feed_id.to_string()))?;
        ensure_current_compatibility(&profile.compatibility)?;
        let allowed = state.can_follow(self, feed_id, profile.visibility);
        if !allowed {
            tracing::warn!(
                peer_id = %self.peer_id,
                %feed_id,
                visibility = ?profile.visibility,
                "p2p follow denied"
            );
            return Err(P2pError::SubscriptionDenied(feed_id.to_string()));
        }
        let visibility = profile.visibility;
        state
            .subscriptions
            .entry(feed_id.to_string())
            .or_default()
            .insert(self.peer_id.clone());
        state
            .participants
            .entry(self.peer_id.clone())
            .or_insert_with(|| PeerParticipation::new(self))
            .roles
            .insert(PeerRole::Subscriber);
        state
            .inboxes
            .entry(self.peer_id.clone())
            .or_insert_with(VecDeque::new);
        tracing::info!(
            peer_id = %self.peer_id,
            %feed_id,
            visibility = ?visibility,
            "p2p feed followed"
        );
        Ok(())
    }

    pub fn grant_subscription(&self, feed_id: &str, subscriber: &PeerNode) -> Result<(), P2pError> {
        self.ensure_compatible()?;
        subscriber.ensure_compatible()?;
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let profile = state
            .feeds
            .get(feed_id)
            .ok_or_else(|| P2pError::FeedNotFound(feed_id.to_string()))?;
        ensure_current_compatibility(&profile.compatibility)?;
        if profile.peer_id != self.peer_id {
            tracing::warn!(
                publisher_peer_id = %self.peer_id,
                feed_owner_peer_id = %profile.peer_id,
                %feed_id,
                "p2p subscription grant denied for non-owner"
            );
            return Err(P2pError::SubscriptionDenied(feed_id.to_string()));
        }
        state
            .grants
            .insert((feed_id.to_string(), subscriber.peer_id.clone()));
        tracing::info!(
            publisher_peer_id = %self.peer_id,
            subscriber_peer_id = %subscriber.peer_id,
            %feed_id,
            "p2p subscription granted"
        );
        Ok(())
    }

    pub fn publish_capsule(&self, signed: Signed<StoryCapsule>) -> Result<usize, P2pError> {
        self.ensure_compatible()?;
        if !signed.verify_capsule()? {
            tracing::warn!(
                peer_id = %self.peer_id,
                feed_id = %signed.value.feed_id,
                capsule_id = %signed.value.capsule_id,
                "p2p capsule signature rejected"
            );
            return Err(P2pError::InvalidSignature);
        }
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let profile = state
            .feeds
            .get(&signed.value.feed_id)
            .ok_or_else(|| P2pError::FeedNotFound(signed.value.feed_id.clone()))?;
        ensure_current_compatibility(&profile.compatibility)?;
        if profile.peer_id != self.peer_id {
            tracing::warn!(
                publisher_peer_id = %self.peer_id,
                feed_owner_peer_id = %profile.peer_id,
                feed_id = %signed.value.feed_id,
                capsule_id = %signed.value.capsule_id,
                "p2p capsule publish denied for non-owner"
            );
            return Err(P2pError::SubscriptionDenied(signed.value.feed_id.clone()));
        }
        let visibility = profile.visibility;
        let feed_id = signed.value.feed_id.clone();
        let capsule_id = signed.value.capsule_id.clone();
        let seq = signed.value.seq;
        let score = signed.value.score;
        let story_kind = signed.value.story_kind;
        let history_capacity = state.history_capacity;
        let history = state.history.entry(feed_id.clone()).or_default();
        if capsule_is_duplicate(history, &signed.value) {
            tracing::debug!(
                publisher_peer_id = %self.peer_id,
                %feed_id,
                %capsule_id,
                seq,
                score,
                story_kind = ?story_kind,
                "p2p capsule suppressed as duplicate"
            );
            return Ok(0);
        }
        history.push_back(signed.clone());
        while history.len() > history_capacity {
            history.pop_front();
        }
        let subscribers = state
            .subscriptions
            .get(&feed_id)
            .cloned()
            .unwrap_or_default();
        let mut delivered = 0usize;
        for subscriber in subscribers {
            if !state.can_receive_peer(&feed_id, &subscriber, visibility) {
                continue;
            }
            state
                .inboxes
                .entry(subscriber)
                .or_insert_with(VecDeque::new)
                .push_back(signed.clone());
            delivered += 1;
        }
        tracing::info!(
            publisher_peer_id = %self.peer_id,
            %feed_id,
            %capsule_id,
            seq,
            score,
            story_kind = ?story_kind,
            delivered,
            visibility = ?visibility,
            "p2p capsule published"
        );
        Ok(delivered)
    }

    pub fn feed_snapshot(
        &self,
        feed_id: &str,
        limit: usize,
    ) -> Result<Vec<Signed<StoryCapsule>>, P2pError> {
        let state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let Some(profile) = state.feeds.get(feed_id) else {
            tracing::warn!(peer_id = %self.peer_id, %feed_id, "p2p snapshot requested for unknown feed");
            return Err(P2pError::FeedNotFound(feed_id.to_string()));
        };
        ensure_current_compatibility(&profile.compatibility)?;
        let history = state.history.get(feed_id).cloned().unwrap_or_default();
        let keep = limit.min(history.len());
        let skip = history.len().saturating_sub(keep);
        tracing::debug!(
            peer_id = %self.peer_id,
            %feed_id,
            requested_limit = limit,
            returned = keep,
            "p2p feed snapshot served"
        );
        Ok(history.into_iter().skip(skip).collect())
    }

    pub fn drain(&self) -> Result<Vec<Signed<StoryCapsule>>, P2pError> {
        self.ensure_compatible()?;
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let inbox = state
            .inboxes
            .entry(self.peer_id.clone())
            .or_insert_with(VecDeque::new);
        let drained: Vec<_> = inbox.drain(..).collect();
        tracing::debug!(
            peer_id = %self.peer_id,
            capsules = drained.len(),
            "p2p inbox drained"
        );
        Ok(drained)
    }

    pub fn known_peers(&self) -> Result<Vec<PeerIdString>, P2pError> {
        self.ensure_compatible()?;
        let state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        Ok(state.participants.keys().cloned().collect())
    }

    pub fn participation(&self, peer_id: &str) -> Result<Option<PeerParticipation>, P2pError> {
        self.ensure_compatible()?;
        let state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        Ok(state.participants.get(peer_id).cloned())
    }

    pub fn browser_handoff_peers(&self) -> Result<Vec<PeerParticipation>, P2pError> {
        self.ensure_compatible()?;
        let state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        Ok(state
            .participants
            .values()
            .filter(|peer| peer.roles.contains(&PeerRole::BrowserHandoff))
            .cloned()
            .collect())
    }

    fn ensure_compatible(&self) -> Result<(), P2pError> {
        let local = ProtocolCompatibility::current();
        if local.is_compatible_with(&self.compatibility) {
            Ok(())
        } else {
            Err(P2pError::IncompatibleProtocol(
                local.status_with(&self.compatibility).message,
            ))
        }
    }
}

fn capsule_is_duplicate(history: &VecDeque<Signed<StoryCapsule>>, capsule: &StoryCapsule) -> bool {
    if capsule.score >= 90 {
        return false;
    }
    history.iter().rev().take(24).any(|recent| {
        if recent.value.story_kind != capsule.story_kind {
            return false;
        }
        let headline_score = headline_similarity(&recent.value.headline, &capsule.headline);
        if headline_score < 88 {
            return false;
        }
        headline_score == 100 || headline_similarity(&recent.value.deck, &capsule.deck) >= 82
    })
}

#[derive(Clone, Debug)]
struct NetworkState {
    feeds: BTreeMap<FeedId, FeedProfile>,
    directory: BTreeMap<GithubUserId, BTreeMap<FeedId, FeedDirectoryEntry>>,
    org_directory: BTreeMap<GithubOrgName, BTreeMap<FeedId, FeedDirectoryEntry>>,
    team_directory: BTreeMap<(GithubOrgName, GithubTeamSlug), BTreeMap<FeedId, FeedDirectoryEntry>>,
    participants: BTreeMap<PeerIdString, PeerParticipation>,
    topics: BTreeMap<FeedId, String>,
    feed_access: BTreeMap<FeedId, FeedAccessPolicy>,
    org_access: BTreeMap<PeerIdString, PeerOrgAccess>,
    subscriptions: BTreeMap<FeedId, BTreeSet<PeerIdString>>,
    grants: BTreeSet<(FeedId, PeerIdString)>,
    inboxes: BTreeMap<PeerIdString, VecDeque<Signed<StoryCapsule>>>,
    history: BTreeMap<FeedId, VecDeque<Signed<StoryCapsule>>>,
    history_capacity: usize,
}

#[derive(Clone, Debug)]
struct PeerOrgAccess {
    org: GithubOrgName,
    teams: BTreeSet<GithubTeamSlug>,
}

impl Default for NetworkState {
    fn default() -> Self {
        Self {
            feeds: BTreeMap::new(),
            directory: BTreeMap::new(),
            org_directory: BTreeMap::new(),
            team_directory: BTreeMap::new(),
            participants: BTreeMap::new(),
            topics: BTreeMap::new(),
            feed_access: BTreeMap::new(),
            org_access: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            grants: BTreeSet::new(),
            inboxes: BTreeMap::new(),
            history: BTreeMap::new(),
            history_capacity: 64,
        }
    }
}

impl NetworkState {
    fn index_directory_entry(&mut self, entry: FeedDirectoryEntry) {
        let feed_id = entry.feed_id.clone();
        self.feed_access
            .insert(feed_id.clone(), entry.access.clone());
        self.directory
            .entry(entry.owner.github_user_id)
            .or_default()
            .insert(feed_id.clone(), entry.clone());
        if let Some(org) = entry.access.github_org.clone() {
            self.org_directory
                .entry(org.clone())
                .or_default()
                .insert(feed_id.clone(), entry.clone());
            if let Some(team) = entry.access.github_team.clone() {
                self.team_directory
                    .entry((org, team))
                    .or_default()
                    .insert(feed_id, entry);
            }
        }
    }

    fn can_follow(&self, peer: &PeerNode, feed_id: &str, visibility: FeedVisibility) -> bool {
        if visibility == FeedVisibility::Public {
            return true;
        }
        if self
            .grants
            .contains(&(feed_id.to_string(), peer.peer_id.clone()))
        {
            return true;
        }
        let Some(access) = self.feed_access.get(feed_id) else {
            return false;
        };
        let Some(peer_access) = self.org_access.get(&peer.peer_id) else {
            return false;
        };
        match visibility {
            FeedVisibility::GithubOrg => access
                .github_org
                .as_ref()
                .is_some_and(|org| org == &peer_access.org),
            FeedVisibility::GithubTeam => {
                access
                    .github_org
                    .as_ref()
                    .is_some_and(|org| org == &peer_access.org)
                    && access
                        .github_team
                        .as_ref()
                        .is_some_and(|team| peer_access.teams.contains(team))
            }
            _ => false,
        }
    }

    fn can_receive_peer(
        &self,
        feed_id: &str,
        peer_id: &PeerIdString,
        visibility: FeedVisibility,
    ) -> bool {
        if visibility == FeedVisibility::Public {
            return true;
        }
        if self
            .grants
            .contains(&(feed_id.to_string(), peer_id.clone()))
        {
            return true;
        }
        let Some(access) = self.feed_access.get(feed_id) else {
            return false;
        };
        let Some(peer_access) = self.org_access.get(peer_id) else {
            return false;
        };
        match visibility {
            FeedVisibility::GithubOrg => access
                .github_org
                .as_ref()
                .is_some_and(|org| org == &peer_access.org),
            FeedVisibility::GithubTeam => {
                access
                    .github_org
                    .as_ref()
                    .is_some_and(|org| org == &peer_access.org)
                    && access
                        .github_team
                        .as_ref()
                        .is_some_and(|team| peer_access.teams.contains(team))
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_feed_core::{AgentEvent, EventKind, SourceKind};
    use agent_feed_directory::{FeedDirectoryEntry, GithubPrincipal};
    use agent_feed_identity::GithubUserId;
    use agent_feed_p2p_proto::{
        FeedVisibility, ProtoError, ProtocolCompatibility, Signed, StoryCapsule,
    };
    use agent_feed_story::compile_events;
    use time::OffsetDateTime;

    fn story_event(kind: EventKind) -> AgentEvent {
        let mut event = AgentEvent::new(SourceKind::Codex, kind, "codex patch applied");
        event.agent = "codex".to_string();
        event.project = Some("agent_feed".to_string());
        event.session_id = Some("session".to_string());
        event.turn_id = Some("turn".to_string());
        event.files = vec!["src/lib.rs".to_string()];
        event.summary = Some("1 changed files. raw diff omitted.".to_string());
        event.score_hint = Some(82);
        event
    }

    fn capsule(feed_id: &str, seq: u64) -> Result<Signed<StoryCapsule>, ProtoError> {
        capsule_with_score(feed_id, seq, 82)
    }

    fn capsule_with_score(
        feed_id: &str,
        seq: u64,
        score: u8,
    ) -> Result<Signed<StoryCapsule>, ProtoError> {
        let mut event = story_event(EventKind::FileChanged);
        event.score_hint = Some(score);
        let mut stories = compile_events([event]);
        let story = stories.remove(0);
        Signed::sign_capsule(
            StoryCapsule::from_story(feed_id, seq, "github:1", &story)?,
            "peer-a",
        )
    }

    fn capsule_from_entry(
        entry: &FeedDirectoryEntry,
        seq: u64,
    ) -> Result<Signed<StoryCapsule>, ProtoError> {
        let mut stories = compile_events([story_event(EventKind::TurnComplete)]);
        let story = stories.remove(0);
        Signed::sign_capsule(
            StoryCapsule::from_story(&entry.feed_id, seq, "github:1", &story)?
                .with_publisher(entry.publisher_identity())?,
            "peer-a",
        )
    }

    fn profile(feed_id: &str, visibility: FeedVisibility) -> Result<FeedProfile, ProtoError> {
        FeedProfile::new(
            feed_id,
            "agent-feed-mainnet",
            "github:1",
            "peer-a",
            "workstation",
            visibility,
        )
        .sign("peer-a")
    }

    fn directory_entry(
        feed_id: &str,
        label: &str,
        visibility: FeedVisibility,
    ) -> Result<FeedDirectoryEntry, Box<dyn std::error::Error>> {
        let owner = GithubPrincipal {
            github_user_id: GithubUserId::new(1),
            current_login: "mosure".to_string(),
            display_name: Some("mosure".to_string()),
            avatar: Some("/avatar/github/1".to_string()),
            verified_by: "edge".to_string(),
            verified_at: OffsetDateTime::now_utc(),
        };
        Ok(FeedDirectoryEntry::new(
            "agent-feed-mainnet",
            feed_id,
            owner,
            "peer-a",
            label,
            visibility,
            1,
        )
        .sign("peer-a")?)
    }

    fn org_directory_entry(
        feed_id: &str,
        label: &str,
        peer_id: &str,
        team: Option<&str>,
    ) -> Result<FeedDirectoryEntry, Box<dyn std::error::Error>> {
        let owner = GithubPrincipal {
            github_user_id: GithubUserId::new(1),
            current_login: "mosure".to_string(),
            display_name: Some("mosure".to_string()),
            avatar: Some("/avatar/github/1".to_string()),
            verified_by: "edge".to_string(),
            verified_at: OffsetDateTime::now_utc(),
        };
        let entry = FeedDirectoryEntry::new(
            "agent-feed-mainnet",
            feed_id,
            owner,
            peer_id,
            label,
            FeedVisibility::GithubOrg,
            1,
        );
        let entry = if let Some(team) = team {
            entry.with_github_team("aberration-technology", team)?
        } else {
            entry.with_github_org("aberration-technology")?
        };
        Ok(entry.sign(peer_id)?)
    }

    #[test]
    fn two_native_peers_exchange_public_capsules() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let subscriber = network.peer("agent-feed-mainnet", "peer-b", "github:2");
        publisher.announce_feed(profile("feed-public", FeedVisibility::Public)?)?;
        subscriber.follow("feed-public")?;

        let delivered = publisher.publish_capsule(capsule("feed-public", 1)?)?;
        let inbox = subscriber.drain()?;

        assert_eq!(delivered, 1);
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].verify_capsule()?);
        Ok(())
    }

    #[test]
    fn private_feed_requires_grant() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let subscriber = network.peer("agent-feed-mainnet", "peer-b", "github:2");
        publisher.announce_feed(profile("feed-private", FeedVisibility::Private)?)?;

        assert!(subscriber.follow("feed-private").is_err());
        publisher.grant_subscription("feed-private", &subscriber)?;
        subscriber.follow("feed-private")?;
        publisher.publish_capsule(capsule("feed-private", 1)?)?;

        assert_eq!(subscriber.drain()?.len(), 1);
        Ok(())
    }

    #[test]
    fn denied_subscriber_cannot_receive_private_capsule() -> Result<(), Box<dyn std::error::Error>>
    {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let denied = network.peer("agent-feed-mainnet", "peer-denied", "github:3");
        publisher.announce_feed(profile("feed-private", FeedVisibility::Private)?)?;

        assert!(denied.follow("feed-private").is_err());
        publisher.publish_capsule(capsule("feed-private", 1)?)?;

        assert!(denied.drain()?.is_empty());
        Ok(())
    }

    #[test]
    fn tampered_capsule_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        publisher.announce_feed(profile("feed-public", FeedVisibility::Public)?)?;
        let mut signed = capsule("feed-public", 1)?;
        signed.value.deck = "raw prompt leaked".to_string();

        assert!(matches!(
            publisher.publish_capsule(signed),
            Err(P2pError::InvalidSignature)
        ));
        Ok(())
    }

    #[test]
    fn capsules_do_not_carry_raw_agent_output() -> Result<(), Box<dyn std::error::Error>> {
        let mut event = story_event(EventKind::FileChanged);
        event.summary = Some("changed files. raw diff omitted. stdout secret omitted.".to_string());
        let story = compile_events([event]).remove(0);
        let signed = Signed::sign_capsule(
            StoryCapsule::from_story("feed-public", 1, "github:1", &story)?,
            "peer-a",
        )?;

        assert!(!signed.value.headline.contains("stdout"));
        assert!(!signed.value.deck.contains("secret"));
        assert!(!signed.value.deck.contains("diff --git"));
        Ok(())
    }

    #[test]
    fn github_user_discovery_returns_signed_feed_records() -> Result<(), Box<dyn std::error::Error>>
    {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let subscriber = network.peer("agent-feed-mainnet", "peer-b", "github:2");
        publisher.announce_directory_entry(directory_entry(
            "feed-workstation",
            "workstation",
            FeedVisibility::Public,
        )?)?;

        let entries = subscriber.discover_github_user(GithubUserId::new(1))?;

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].feed_label, "workstation");
        assert!(entries[0].verify_signature()?);
        assert!(!entries[0].live_topic.contains("mosure"));
        Ok(())
    }

    #[test]
    fn incompatible_peer_cannot_join_fabric() {
        let network = InMemoryNetwork::new();
        let stale = network.peer_with_compatibility(
            "agent-feed-mainnet",
            "peer-old",
            "github:1",
            ProtocolCompatibility::current().with_model_version(2, 2),
        );

        let err = stale
            .join_fabric([PeerRole::Rendezvous])
            .expect_err("incompatible peer is rejected");

        assert!(matches!(err, P2pError::IncompatibleProtocol(_)));
        assert!(err.to_string().contains("update your peer"));
    }

    #[test]
    fn incompatible_feed_profile_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let mut feed = profile("feed-public", FeedVisibility::Public)?;
        feed.compatibility = ProtocolCompatibility::current().with_model_version(2, 2);
        feed = feed.sign("peer-a")?;

        let err = publisher
            .announce_feed(feed)
            .expect_err("incompatible feed is rejected");

        assert!(matches!(
            err,
            P2pError::Directory(DirectoryError::IncompatibleProtocol(_))
        ));
        assert!(err.to_string().contains("update your peer"));
        Ok(())
    }

    #[test]
    fn incompatible_directory_entry_is_not_cached() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let fabric = network.peer("agent-feed-mainnet", "peer-fabric", "fabric:edge");
        let mut entry = directory_entry("feed-public", "workstation", FeedVisibility::Public)?;
        entry.compatibility = ProtocolCompatibility::current().with_model_version(2, 2);
        entry = entry.sign("peer-a")?;

        let err = fabric
            .cache_directory_entry(entry)
            .expect_err("incompatible directory entry is rejected");

        assert!(matches!(
            err,
            P2pError::Directory(DirectoryError::IncompatibleProtocol(_))
        ));
        assert!(
            fabric
                .discover_github_user(GithubUserId::new(1))?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn fabric_peer_can_route_without_subscribing() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let fabric = network.peer("agent-feed-mainnet", "peer-fabric", "fabric:edge");
        let entry = directory_entry("feed-public", "workstation", FeedVisibility::Public)?;
        publisher.announce_feed(profile("feed-public", FeedVisibility::Public)?)?;
        fabric.join_fabric([PeerRole::Rendezvous])?;
        fabric.cache_directory_entry(entry.clone())?;

        let entries = fabric.discover_github_user(GithubUserId::new(1))?;
        let delivered = publisher.publish_capsule(capsule_from_entry(&entry, 1)?)?;
        let participation = fabric
            .participation("peer-fabric")?
            .expect("fabric peer is registered");

        assert_eq!(entries.len(), 1);
        assert_eq!(delivered, 0);
        assert!(fabric.drain()?.is_empty());
        assert!(participation.roles.contains(&PeerRole::Fabric));
        assert!(participation.roles.contains(&PeerRole::KadProvider));
        assert!(!participation.is_subscriber());
        Ok(())
    }

    #[test]
    fn browser_handoff_peer_is_not_feed_subscriber() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let handoff = network.peer("agent-feed-mainnet", "peer-webrtc", "fabric:webrtc");
        let subscriber = network.peer("agent-feed-mainnet", "peer-b", "github:2");
        let entry = directory_entry("feed-public", "workstation", FeedVisibility::Public)?;
        publisher.announce_feed(profile("feed-public", FeedVisibility::Public)?)?;
        handoff.register_browser_handoff([
            "/dns4/edge.agent-feed.example/udp/443/webrtc-direct".to_string(),
        ])?;
        handoff.cache_directory_entry(entry.clone())?;
        subscriber.follow("feed-public")?;

        let delivered = publisher.publish_capsule(capsule_from_entry(&entry, 1)?)?;
        let handoff_peers = subscriber.browser_handoff_peers()?;
        let handoff_state = subscriber
            .participation("peer-webrtc")?
            .expect("handoff peer is registered");

        assert_eq!(delivered, 1);
        assert!(handoff.drain()?.is_empty());
        assert_eq!(subscriber.drain()?.len(), 1);
        assert_eq!(handoff_peers.len(), 1);
        assert!(handoff_state.roles.contains(&PeerRole::BrowserHandoff));
        assert!(!handoff_state.is_subscriber());
        Ok(())
    }

    #[test]
    fn org_feed_discovery_does_not_auto_subscribe() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let fabric = network.peer("agent-feed-mainnet", "peer-fabric", "fabric:edge");
        let member = network.peer("agent-feed-mainnet", "peer-member", "github:2");
        let org = GithubOrgName::parse("aberration-technology")?;
        let entry = org_directory_entry("feed-org", "workstation", "peer-a", None)?;
        publisher.announce_feed(profile("feed-org", FeedVisibility::GithubOrg)?)?;
        publisher.announce_directory_entry(entry.clone())?;
        fabric.join_fabric([PeerRole::Rendezvous, PeerRole::KadProvider])?;
        fabric.cache_directory_entry(entry.clone())?;

        let discovered = member.discover_github_org(&org)?;
        let delivered_before_follow = publisher.publish_capsule(capsule_from_entry(&entry, 1)?)?;

        assert_eq!(discovered.len(), 1);
        assert_eq!(delivered_before_follow, 0);
        assert!(member.drain()?.is_empty());
        assert!(fabric.drain()?.is_empty());
        assert!(
            !fabric
                .participation("peer-fabric")?
                .expect("fabric participates")
                .is_subscriber()
        );
        Ok(())
    }

    #[test]
    fn org_member_can_explicitly_follow_org_feed() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let member = network.peer("agent-feed-mainnet", "peer-member", "github:2");
        let outsider = network.peer("agent-feed-mainnet", "peer-outsider", "github:3");
        let org = GithubOrgName::parse("aberration-technology")?;
        let entry = org_directory_entry("feed-org", "workstation", "peer-a", None)?;
        publisher.announce_feed(profile("feed-org", FeedVisibility::GithubOrg)?)?;
        publisher.announce_directory_entry(entry.clone())?;
        member.certify_github_org_access(org, [])?;

        assert!(outsider.follow("feed-org").is_err());
        member.follow("feed-org")?;
        let delivered = publisher.publish_capsule(capsule_from_entry(&entry, 1)?)?;

        assert_eq!(delivered, 1);
        assert_eq!(member.drain()?.len(), 1);
        assert!(outsider.drain()?.is_empty());
        Ok(())
    }

    #[test]
    fn team_feed_requires_matching_team_membership() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let release_member = network.peer("agent-feed-mainnet", "peer-release", "github:2");
        let lab_member = network.peer("agent-feed-mainnet", "peer-lab", "github:3");
        let org = GithubOrgName::parse("aberration-technology")?;
        let release = GithubTeamSlug::parse("release")?;
        let lab = GithubTeamSlug::parse("lab")?;
        let entry = org_directory_entry("feed-release", "release", "peer-a", Some("release"))?;
        publisher.announce_feed(profile("feed-release", FeedVisibility::GithubTeam)?)?;
        publisher.announce_directory_entry(entry.clone())?;
        release_member.certify_github_org_access(org.clone(), [release.clone()])?;
        lab_member.certify_github_org_access(org.clone(), [lab])?;

        let discovered = lab_member.discover_github_team(&org, &release)?;

        assert_eq!(discovered.len(), 1);
        assert!(lab_member.follow("feed-release").is_err());
        release_member.follow("feed-release")?;
        assert_eq!(
            publisher.publish_capsule(capsule_from_entry(&entry, 1)?)?,
            1
        );
        assert_eq!(release_member.drain()?.len(), 1);
        assert!(lab_member.drain()?.is_empty());
        Ok(())
    }

    #[test]
    fn delivered_capsule_preserves_github_publisher_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let subscriber = network.peer("agent-feed-mainnet", "peer-b", "github:2");
        let entry = directory_entry("feed-public", "workstation", FeedVisibility::Public)?;
        publisher.announce_feed(profile("feed-public", FeedVisibility::Public)?)?;
        publisher.announce_directory_entry(entry.clone())?;
        subscriber.follow("feed-public")?;

        publisher.publish_capsule(capsule_from_entry(&entry, 1)?)?;
        let inbox = subscriber.drain()?;
        let publisher_identity = inbox[0]
            .value
            .publisher
            .as_ref()
            .expect("publisher identity is present");

        assert_eq!(publisher_identity.github_login.as_deref(), Some("mosure"));
        assert_eq!(
            publisher_identity.avatar.as_deref(),
            Some("/avatar/github/1")
        );
        assert!(publisher_identity.verified);
        Ok(())
    }

    #[test]
    fn published_feed_keeps_ring_buffer_snapshot_without_subscribers()
    -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        publisher.announce_feed(profile("feed-public", FeedVisibility::Public)?)?;

        for seq in 1..=5 {
            assert_eq!(
                publisher.publish_capsule(capsule_with_score("feed-public", seq, 95)?)?,
                0
            );
        }

        let snapshot = publisher.feed_snapshot("feed-public", 3)?;

        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].value.seq, 3);
        assert_eq!(snapshot[1].value.seq, 4);
        assert_eq!(snapshot[2].value.seq, 5);
        Ok(())
    }

    #[test]
    fn duplicate_headline_capsule_is_not_delivered_or_snapshotted()
    -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-feed-mainnet", "peer-a", "github:1");
        let subscriber = network.peer("agent-feed-mainnet", "peer-b", "github:2");
        publisher.announce_feed(profile("feed-public", FeedVisibility::Public)?)?;
        subscriber.follow("feed-public")?;

        assert_eq!(publisher.publish_capsule(capsule("feed-public", 1)?)?, 1);
        assert_eq!(publisher.publish_capsule(capsule("feed-public", 2)?)?, 0);

        let inbox = subscriber.drain()?;
        let snapshot = publisher.feed_snapshot("feed-public", 10)?;
        assert_eq!(inbox.len(), 1);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].value.seq, 1);
        Ok(())
    }
}
