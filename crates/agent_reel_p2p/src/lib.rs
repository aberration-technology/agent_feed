use agent_reel_directory::{DirectoryError, FeedDirectoryEntry};
use agent_reel_identity::GithubUserId;
use agent_reel_p2p_proto::{
    FeedId, FeedProfile, FeedVisibility, NetworkId, PeerIdString, Signed, StoryCapsule, feed_topic,
};
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
    pub principal: String,
    pub roles: BTreeSet<PeerRole>,
    pub browser_handoff_addrs: Vec<String>,
}

impl PeerParticipation {
    fn new(peer: &PeerNode) -> Self {
        Self {
            network_id: peer.network_id.clone(),
            peer_id: peer.peer_id.clone(),
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
    #[error(transparent)]
    Proto(#[from] agent_reel_p2p_proto::ProtoError),
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
            principal: principal.into(),
            network: self.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PeerNode {
    pub network_id: NetworkId,
    pub peer_id: PeerIdString,
    pub principal: String,
    network: InMemoryNetwork,
}

impl PeerNode {
    pub fn join_fabric<I>(&self, roles: I) -> Result<(), P2pError>
    where
        I: IntoIterator<Item = PeerRole>,
    {
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
        state
            .inboxes
            .entry(self.peer_id.clone())
            .or_insert_with(VecDeque::new);
        Ok(())
    }

    pub fn register_browser_handoff<I>(&self, addrs: I) -> Result<(), P2pError>
    where
        I: IntoIterator<Item = String>,
    {
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
        state
            .inboxes
            .entry(self.peer_id.clone())
            .or_insert_with(VecDeque::new);
        Ok(())
    }

    pub fn announce_feed(&self, profile: FeedProfile) -> Result<(), P2pError> {
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        state.topics.insert(
            profile.feed_id.clone(),
            feed_topic(&profile.network_id, &profile.feed_id),
        );
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
        Ok(())
    }

    pub fn announce_directory_entry(&self, entry: FeedDirectoryEntry) -> Result<(), P2pError> {
        if !entry.verify_signature()? {
            return Err(P2pError::Directory(DirectoryError::InvalidSignature));
        }
        if entry.peer_id != self.peer_id {
            return Err(P2pError::SubscriptionDenied(entry.feed_id));
        }
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        state
            .directory
            .entry(entry.owner.github_user_id)
            .or_default()
            .insert(entry.feed_id.clone(), entry);
        state
            .participants
            .entry(self.peer_id.clone())
            .or_insert_with(|| PeerParticipation::new(self))
            .roles
            .insert(PeerRole::KadProvider);
        Ok(())
    }

    pub fn cache_directory_entry(&self, entry: FeedDirectoryEntry) -> Result<(), P2pError> {
        if !entry.verify_signature()? {
            return Err(P2pError::Directory(DirectoryError::InvalidSignature));
        }
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        state
            .directory
            .entry(entry.owner.github_user_id)
            .or_default()
            .insert(entry.feed_id.clone(), entry);
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
        Ok(())
    }

    pub fn discover_github_user(
        &self,
        github_user_id: GithubUserId,
    ) -> Result<Vec<FeedDirectoryEntry>, P2pError> {
        let state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        Ok(state
            .directory
            .get(&github_user_id)
            .map(|entries| entries.values().cloned().collect())
            .unwrap_or_default())
    }

    pub fn follow(&self, feed_id: &str) -> Result<(), P2pError> {
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let profile = state
            .feeds
            .get(feed_id)
            .ok_or_else(|| P2pError::FeedNotFound(feed_id.to_string()))?;
        let allowed = profile.visibility == FeedVisibility::Public
            || state
                .grants
                .contains(&(feed_id.to_string(), self.peer_id.clone()));
        if !allowed {
            return Err(P2pError::SubscriptionDenied(feed_id.to_string()));
        }
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
        Ok(())
    }

    pub fn grant_subscription(&self, feed_id: &str, subscriber: &PeerNode) -> Result<(), P2pError> {
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let profile = state
            .feeds
            .get(feed_id)
            .ok_or_else(|| P2pError::FeedNotFound(feed_id.to_string()))?;
        if profile.peer_id != self.peer_id {
            return Err(P2pError::SubscriptionDenied(feed_id.to_string()));
        }
        state
            .grants
            .insert((feed_id.to_string(), subscriber.peer_id.clone()));
        Ok(())
    }

    pub fn publish_capsule(&self, signed: Signed<StoryCapsule>) -> Result<usize, P2pError> {
        if !signed.verify_capsule()? {
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
        if profile.peer_id != self.peer_id {
            return Err(P2pError::SubscriptionDenied(signed.value.feed_id.clone()));
        }
        let visibility = profile.visibility;
        let feed_id = signed.value.feed_id.clone();
        let history_capacity = state.history_capacity;
        let history = state.history.entry(feed_id.clone()).or_default();
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
            let can_receive = visibility == FeedVisibility::Public
                || state
                    .grants
                    .contains(&(feed_id.clone(), subscriber.clone()));
            if !can_receive {
                continue;
            }
            state
                .inboxes
                .entry(subscriber)
                .or_insert_with(VecDeque::new)
                .push_back(signed.clone());
            delivered += 1;
        }
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
        if !state.feeds.contains_key(feed_id) {
            return Err(P2pError::FeedNotFound(feed_id.to_string()));
        }
        let history = state.history.get(feed_id).cloned().unwrap_or_default();
        let keep = limit.min(history.len());
        let skip = history.len().saturating_sub(keep);
        Ok(history.into_iter().skip(skip).collect())
    }

    pub fn drain(&self) -> Result<Vec<Signed<StoryCapsule>>, P2pError> {
        let mut state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        let inbox = state
            .inboxes
            .entry(self.peer_id.clone())
            .or_insert_with(VecDeque::new);
        Ok(inbox.drain(..).collect())
    }

    pub fn known_peers(&self) -> Result<Vec<PeerIdString>, P2pError> {
        let state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        Ok(state.participants.keys().cloned().collect())
    }

    pub fn participation(&self, peer_id: &str) -> Result<Option<PeerParticipation>, P2pError> {
        let state = self
            .network
            .state
            .lock()
            .map_err(|_| P2pError::StatePoisoned)?;
        Ok(state.participants.get(peer_id).cloned())
    }

    pub fn browser_handoff_peers(&self) -> Result<Vec<PeerParticipation>, P2pError> {
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
}

#[derive(Clone, Debug)]
struct NetworkState {
    feeds: BTreeMap<FeedId, FeedProfile>,
    directory: BTreeMap<GithubUserId, BTreeMap<FeedId, FeedDirectoryEntry>>,
    participants: BTreeMap<PeerIdString, PeerParticipation>,
    topics: BTreeMap<FeedId, String>,
    subscriptions: BTreeMap<FeedId, BTreeSet<PeerIdString>>,
    grants: BTreeSet<(FeedId, PeerIdString)>,
    inboxes: BTreeMap<PeerIdString, VecDeque<Signed<StoryCapsule>>>,
    history: BTreeMap<FeedId, VecDeque<Signed<StoryCapsule>>>,
    history_capacity: usize,
}

impl Default for NetworkState {
    fn default() -> Self {
        Self {
            feeds: BTreeMap::new(),
            directory: BTreeMap::new(),
            participants: BTreeMap::new(),
            topics: BTreeMap::new(),
            subscriptions: BTreeMap::new(),
            grants: BTreeSet::new(),
            inboxes: BTreeMap::new(),
            history: BTreeMap::new(),
            history_capacity: 64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_reel_core::{AgentEvent, EventKind, SourceKind};
    use agent_reel_directory::{FeedDirectoryEntry, GithubPrincipal};
    use agent_reel_identity::GithubUserId;
    use agent_reel_p2p_proto::{FeedVisibility, ProtoError, Signed, StoryCapsule};
    use agent_reel_story::compile_events;
    use time::OffsetDateTime;

    fn story_event(kind: EventKind) -> AgentEvent {
        let mut event = AgentEvent::new(SourceKind::Codex, kind, "codex patch applied");
        event.agent = "codex".to_string();
        event.project = Some("agent_reel".to_string());
        event.session_id = Some("session".to_string());
        event.turn_id = Some("turn".to_string());
        event.files = vec!["src/lib.rs".to_string()];
        event.summary = Some("1 changed files. raw diff omitted.".to_string());
        event.score_hint = Some(82);
        event
    }

    fn capsule(feed_id: &str, seq: u64) -> Result<Signed<StoryCapsule>, ProtoError> {
        let mut stories = compile_events([story_event(EventKind::FileChanged)]);
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
            "agent-reel-mainnet",
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
            "agent-reel-mainnet",
            feed_id,
            owner,
            "peer-a",
            label,
            visibility,
            1,
        )
        .sign("peer-a")?)
    }

    #[test]
    fn two_native_peers_exchange_public_capsules() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-reel-mainnet", "peer-a", "github:1");
        let subscriber = network.peer("agent-reel-mainnet", "peer-b", "github:2");
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
        let publisher = network.peer("agent-reel-mainnet", "peer-a", "github:1");
        let subscriber = network.peer("agent-reel-mainnet", "peer-b", "github:2");
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
        let publisher = network.peer("agent-reel-mainnet", "peer-a", "github:1");
        let denied = network.peer("agent-reel-mainnet", "peer-denied", "github:3");
        publisher.announce_feed(profile("feed-private", FeedVisibility::Private)?)?;

        assert!(denied.follow("feed-private").is_err());
        publisher.publish_capsule(capsule("feed-private", 1)?)?;

        assert!(denied.drain()?.is_empty());
        Ok(())
    }

    #[test]
    fn tampered_capsule_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-reel-mainnet", "peer-a", "github:1");
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
        let publisher = network.peer("agent-reel-mainnet", "peer-a", "github:1");
        let subscriber = network.peer("agent-reel-mainnet", "peer-b", "github:2");
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
    fn fabric_peer_can_route_without_subscribing() -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-reel-mainnet", "peer-a", "github:1");
        let fabric = network.peer("agent-reel-mainnet", "peer-fabric", "fabric:edge");
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
        let publisher = network.peer("agent-reel-mainnet", "peer-a", "github:1");
        let handoff = network.peer("agent-reel-mainnet", "peer-webrtc", "fabric:webrtc");
        let subscriber = network.peer("agent-reel-mainnet", "peer-b", "github:2");
        let entry = directory_entry("feed-public", "workstation", FeedVisibility::Public)?;
        publisher.announce_feed(profile("feed-public", FeedVisibility::Public)?)?;
        handoff.register_browser_handoff([
            "/dns4/edge.agent-reel.example/udp/443/webrtc-direct".to_string(),
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
    fn delivered_capsule_preserves_github_publisher_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let network = InMemoryNetwork::new();
        let publisher = network.peer("agent-reel-mainnet", "peer-a", "github:1");
        let subscriber = network.peer("agent-reel-mainnet", "peer-b", "github:2");
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
        let publisher = network.peer("agent-reel-mainnet", "peer-a", "github:1");
        publisher.announce_feed(profile("feed-public", FeedVisibility::Public)?)?;

        for seq in 1..=5 {
            assert_eq!(publisher.publish_capsule(capsule("feed-public", seq)?)?, 0);
        }

        let snapshot = publisher.feed_snapshot("feed-public", 3)?;

        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].value.seq, 3);
        assert_eq!(snapshot[1].value.seq, 4);
        assert_eq!(snapshot[2].value.seq, 5);
        Ok(())
    }
}
