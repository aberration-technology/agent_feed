pub use agent_reel_core::{
    AdapterName, AgentEvent, AgentName, Bulletin, BulletinChip, BulletinId, BulletinMode, EventId,
    EventKind, HeadlineImage, ItemId, MaskedCommand, MaskedPath, MaskedUri, PrivacyClass,
    ProjectRef, RawAgentEvent, SessionId, Severity, SourceKind, Tag, TickerItem, ToolRef, TurnId,
    VisualKind,
};
#[cfg(feature = "p2p")]
pub use agent_reel_directory::{
    DirectoryStore, FeedDirectoryEntry, GithubDiscoveryTicket, GithubPrincipal, NetworkSelector,
    ReelLayout, RemoteHeadlineView, RemoteReelFilter, RemoteUserRoute, StreamDescriptor,
    StreamFilter,
};
pub use agent_reel_filter::EventFilter;
pub use agent_reel_highlight::{bulletin_from_event, score_event};
pub use agent_reel_identity::{GithubLogin, GithubUserId, PrincipalRef};
#[cfg(feature = "github")]
pub use agent_reel_identity_github::{GithubProfile, GithubResolver, StaticGithubResolver};
pub use agent_reel_ingest::{GenericIngestEvent, normalize_raw, normalize_value, parse_jsonl};
#[cfg(feature = "p2p")]
pub use agent_reel_p2p::{InMemoryNetwork, P2pError, PeerNode, PeerParticipation, PeerRole};
#[cfg(feature = "p2p-browser")]
pub use agent_reel_p2p_browser::{RemoteFeedHeadline, RemoteRouteState, RemoteRouteViewModel};
#[cfg(feature = "p2p")]
pub use agent_reel_p2p_proto::{
    FeedEnvelope, FeedProfile, FeedVisibility, PublisherIdentity, Signed, StoryCapsule, feed_topic,
    github_provider_key, github_user_topic,
};
pub use agent_reel_redaction::{PrivacyConfig, PrivacyMode, Redactor};
pub use agent_reel_reel::{ReelBuffer, ReelSnapshot};
pub use agent_reel_security::{SecurityConfig, validate_bind};
pub use agent_reel_store::InMemoryStore;
pub use agent_reel_story::{CompiledStory, StoryCompiler, StoryCompilerConfig, compile_events};
pub use agent_reel_summarize::{
    FeedSummary, FeedSummaryMode, ImageConfig, ImageDecisionMode, ImageProcessor,
    ImageProcessorConfig, SummaryBudget, SummaryConfig, SummaryGuardrails, SummaryProcessor,
    SummaryProcessorConfig, summarize_feed, summarize_feed_with_processor,
    summarize_feed_with_processors,
};
