use agent_feed_p2p_proto::{FeedId, FeedProfile, PeerIdString};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowRequest {
    pub feed_id: FeedId,
    pub subscriber_peer_id: PeerIdString,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FeedDirectory {
    pub feeds: Vec<FeedProfile>,
}

impl FeedDirectory {
    pub fn publish(&mut self, profile: FeedProfile) {
        self.feeds.retain(|feed| feed.feed_id != profile.feed_id);
        self.feeds.push(profile);
    }
}
