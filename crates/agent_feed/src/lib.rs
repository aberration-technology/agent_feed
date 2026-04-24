pub use agent_feed_core::{
    AdapterName, AgentEvent, AgentName, Bulletin, BulletinChip, BulletinId, BulletinMode, EventId,
    EventKind, HeadlineImage, ItemId, MaskedCommand, MaskedPath, MaskedUri, PrivacyClass,
    ProjectRef, RawAgentEvent, SessionId, Severity, SourceKind, Tag, TickerItem, ToolRef, TurnId,
    VisualKind,
};
#[cfg(feature = "p2p")]
pub use agent_feed_directory::{
    DirectoryStore, FeedDirectoryEntry, GithubDiscoveryTicket, GithubPrincipal, NetworkSelector,
    ReelLayout, RemoteHeadlineView, RemoteReelFilter, RemoteUserRoute, StreamDescriptor,
    StreamFilter,
};
pub use agent_feed_filter::EventFilter;
pub use agent_feed_highlight::{bulletin_from_event, score_event};
pub use agent_feed_identity::{GithubLogin, GithubUserId, PrincipalRef};
#[cfg(feature = "github")]
pub use agent_feed_identity_github::{GithubProfile, GithubResolver, StaticGithubResolver};
pub use agent_feed_ingest::{GenericIngestEvent, normalize_raw, normalize_value, parse_jsonl};
#[cfg(feature = "p2p")]
pub use agent_feed_p2p::{InMemoryNetwork, P2pError, PeerNode, PeerParticipation, PeerRole};
#[cfg(feature = "p2p-browser")]
pub use agent_feed_p2p_browser::{RemoteFeedHeadline, RemoteRouteState, RemoteRouteViewModel};
#[cfg(feature = "p2p")]
pub use agent_feed_p2p_proto::{
    FeedEnvelope, FeedProfile, FeedVisibility, PublisherIdentity, Signed, StoryCapsule, feed_topic,
    github_provider_key, github_user_topic,
};
pub use agent_feed_redaction::{PrivacyConfig, PrivacyMode, Redactor};
pub use agent_feed_reel::{ReelBuffer, ReelSnapshot};
pub use agent_feed_security::{SecurityConfig, validate_bind};
pub use agent_feed_store::InMemoryStore;
pub use agent_feed_story::{CompiledStory, StoryCompiler, StoryCompilerConfig, compile_events};
pub use agent_feed_summarize::{
    FeedSummary, FeedSummaryMode, ImageConfig, ImageDecisionMode, ImageProcessor,
    ImageProcessorConfig, SummaryBudget, SummaryConfig, SummaryGuardrails, SummaryProcessor,
    SummaryProcessorConfig, summarize_feed, summarize_feed_with_processor,
    summarize_feed_with_processors,
};
