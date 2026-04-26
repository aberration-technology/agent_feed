const INDEX_HTML: &str = include_str!("index.html");
const REEL_CSS: &str = include_str!("reel.css");
const REEL_JS: &str = include_str!("reel.ts");
pub const FAVICON_SVG: &str = include_str!("favicon.svg");

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiConfig {
    pub p2p_enabled: bool,
    pub revision: Option<String>,
}

#[must_use]
pub fn render_index(view: Option<&str>) -> String {
    render_index_with_config(view, &UiConfig::default())
}

#[must_use]
pub fn render_index_with_config(view: Option<&str>, config: &UiConfig) -> String {
    let revision = config
        .revision
        .clone()
        .or_else(default_revision)
        .unwrap_or_else(|| "dev".to_string());
    let config_js = format!(
        "window.FEED_P2P_ENABLED = {};\nwindow.FEED_BUILD_REV = {};\nwindow.FEED_COMPATIBILITY = {};",
        if config.p2p_enabled { "true" } else { "false" },
        js_string(&revision),
        compatibility_js()
    );
    INDEX_HTML
        .replace("/*__REEL_CSS__*/", REEL_CSS)
        .replace("/*__REEL_JS__*/", REEL_JS)
        .replace("/*__FEED_CONFIG__*/", &config_js)
        .replace("__REEL_VIEW__", view.unwrap_or("stage"))
}

fn compatibility_js() -> String {
    format!(
        "{{\"product\":{},\"release_version\":{},\"protocol_version\":{},\"model_version\":{},\"min_model_version\":{}}}",
        js_string(agent_feed_p2p_proto::AGENT_FEED_PRODUCT),
        js_string(agent_feed_p2p_proto::AGENT_FEED_RELEASE_VERSION),
        agent_feed_p2p_proto::AGENT_FEED_PROTOCOL_VERSION,
        agent_feed_p2p_proto::AGENT_FEED_MODEL_VERSION,
        agent_feed_p2p_proto::AGENT_FEED_MIN_MODEL_VERSION
    )
}

fn default_revision() -> Option<String> {
    option_env!("GITHUB_SHA")
        .or(option_env!("VERGEN_GIT_SHA"))
        .map(short_revision)
}

fn short_revision(value: &str) -> String {
    value.chars().take(12).collect::<String>()
}

fn js_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => output.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::{UiConfig, render_index_with_config};

    fn config(p2p_enabled: bool) -> UiConfig {
        UiConfig {
            p2p_enabled,
            revision: Some("abc123def456".to_string()),
        }
    }

    #[test]
    fn stage_progress_starts_hidden() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("id=\"stage-progress\""));
        assert!(html.contains("aria-hidden=\"true\" hidden"));
    }

    #[test]
    fn stage_story_time_is_subtle_and_story_bound() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("class=\"story-time\" id=\"story-time\" hidden"));
        assert!(html.contains(".story-time {\n  position: absolute;"));
        assert!(html.contains("color: rgba(255, 255, 255, 0.16);"));
        assert!(html.contains("function renderStoryTime"));
        assert!(html.contains("storyTime.textContent = `posted ${relativeTime(timestamp)}`;"));
        assert!(html.contains("function clearStoryTime"));
        assert!(html.contains("refreshStoryTime();"));
    }

    #[test]
    fn favicon_matches_feed_mark() {
        let html = render_index_with_config(Some("stage"), &config(false));

        assert!(html.contains("href=\"./favicon.svg\" type=\"image/svg+xml\""));
        assert!(super::FAVICON_SVG.contains("rx=\"12\" fill=\"#000000\""));
        assert!(super::FAVICON_SVG.contains("fill=\"#d87c7c\""));
        assert!(super::FAVICON_SVG.contains("V30h-7v-8h7v-3"));
    }

    #[test]
    fn idle_state_avoids_redundant_local_status_chips() {
        let html = render_index_with_config(Some("stage"), &config(false));

        assert!(
            html.contains(
                "<a class=\"brand\" href=\"https://feed.aberration.technology/\">feed</a>"
            )
        );
        assert!(html.contains("id=\"eyebrow\">local feed</div>"));
        assert!(html.contains("<span>privacy on</span>"));
        assert!(!html.contains("LOCAL / QUIET / IDLE"));
        assert!(!html.contains(
            "<span>local</span>\n          <span>redacted</span>\n          <span>idle</span>"
        ));
        assert!(html.contains("setText(eyebrow, \"p2p disabled\");"));
        assert!(html.contains("renderChips([\"p2p off\", \"privacy on\"]);"));
    }

    #[test]
    fn chrome_uses_lowercase_accented_site_links() {
        let html = render_index_with_config(Some("stage"), &config(false));

        assert!(html.contains("--sans: ui-monospace, monospace;"));
        assert!(html.contains("--mono: ui-monospace, monospace;"));
        assert!(html.contains("font-family: ui-monospace, monospace;"));
        assert!(!html.contains("ui-sans-serif"));
        assert!(!html.contains("SFMono-Regular"));
        assert!(html.contains("--secondary: #d87c7c;"));
        assert!(html.contains(".brand {\n  color: var(--secondary);\n  text-decoration: none;\n}"));
        assert!(html.contains(".brand:hover,\n.brand:focus-visible"));
        assert!(html.contains(".footer-links a {\n  color: var(--secondary);"));
        assert!(html.contains("id=\"footer-rev\">rev dev</span>"));
        assert!(html.contains(".footer-rev"));
        assert!(html.contains("window.FEED_BUILD_REV = \"abc123def456\";"));
        assert!(html.contains("href=\"https://aberration.technology/\""));
        assert!(html.contains("href=\"https://github.com/aberration-technology\""));
        assert!(!html.contains("href=\"https://github.com/aberration-technology/agent_feed\""));
        assert!(html.contains("text-decoration: underline;"));
        assert!(!html.contains("text-transform: uppercase;"));
    }

    #[test]
    fn remote_states_stop_dwell_progress() {
        let html = render_index_with_config(Some("remote"), &config(true));
        let remote_state = html
            .split("function renderRemoteState")
            .nth(1)
            .expect("remote state renderer is embedded");

        assert!(remote_state.contains("stopStageProgress();"));
        assert!(!remote_state.contains("restartStageProgress"));
    }

    #[test]
    fn remote_user_route_shows_identity_once() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function routeStreamLabel"));
        assert!(html.contains("function routeEyebrow"));
        assert!(html.contains("function remoteHeadlineForState"));
        assert!(html.contains("setText(eyebrow, routeEyebrow(route));"));
        assert!(html.contains("setText(headline, remoteHeadlineForState(state));"));
        assert!(html
            .contains("renderPublisher(nextPublisher || (route.kind === \"global\" ? undefined : { login: route.login }));"));
        assert!(!html.contains("`@${route.login} / ${route.selection} / ${route.feedMode}`"));
        assert!(!html.contains("setText(headline, `@${route.login}`);"));
    }

    #[test]
    fn timeline_uses_feed_labels_without_repeating_login() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("feedLink(route.login, \"*\", \"all feeds\""));
        assert!(html.contains("feed.publisher_avatar ||"));
        assert!(html.contains("meta.textContent = feedLabel;"));
        assert!(html.contains("interactive timeline · ${routeStreamLabel(route)}"));
        assert!(!html.contains("feedLink(route.login, \"*\", `${route.login}/*`"));
        assert!(
            !html.contains("meta.textContent = `${publisherText(feed, ticket)} / ${feedLabel}`;")
        );
    }

    #[test]
    fn discovery_publishers_link_to_user_discovery_feed() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function userDiscoveryUrl"));
        assert!(html.contains("return `/${encodeURIComponent(login)}/*"));
        assert!(html.contains("publisher.dataset.href = profileUrl;"));
        assert!(html.contains("open @${login} discovery feed"));
        assert!(html.contains("timelinePublisher(item, { profile: {} }, route)"));
        assert!(
            html.contains("const node = document.createElement(profileUrl ? \"a\" : \"div\");")
        );
        assert!(html.contains(".publisher[data-href]"));
    }

    #[test]
    fn browser_user_route_filters_headlines_without_throwing() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function headlineMatchesRoute"));
        assert!(html.contains("const requestedFeeds = requestedFeedLabels(route);"));
        assert!(html.contains("function requestedFeedLabels"));
        assert!(html.contains("function headlineFeedLabels"));
        assert!(html.contains("clean === `${String(login || \"\").replace(/^@/, \"\")}/*`"));
        assert!(html.contains(".filter((item) => headlineMatchesRoute(item, route))"));
        assert!(html.contains("params.set(\"streams\", \"all\");"));
    }

    #[test]
    fn browser_network_source_count_uses_feed_identity_not_chips() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("source_key:"));
        assert!(html.contains("function bulletinSourceKey"));
        assert!(html.contains("bulletin.feed_id || bulletin.feedId"));
        assert!(html.contains("bulletin.lower_third || bulletin.lowerThird"));
        assert!(!html.contains("const firstChip = bulletin.chips?.[0];\n    const label"));
    }

    #[test]
    fn browser_console_logs_feed_lifecycle_events() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function logEvent"));
        assert!(html.contains("feed.remote.route.start"));
        assert!(html.contains("feed.resolver.response"));
        assert!(html.contains("feed.network.discovery.snapshot"));
        assert!(html.contains("feed.sse.bulletin.incoming"));
    }

    #[test]
    fn browser_remote_route_reports_version_mismatch() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("const FEED_PROTOCOL_VERSION = 1;"));
        assert!(html.contains("window.FEED_COMPATIBILITY = {"));
        assert!(html.contains("\"model_version\":3"));
        assert!(html.contains("const FEED_MODEL_VERSION = Number"));
        assert!(html.contains("function compatibilityStatus"));
        assert!(html.contains("function networkCompatibilityStatus"));
        assert!(html.contains("version-mismatch"));
        assert!(html.contains("update your peer to the latest version"));
        assert!(html.contains("feed.user.incompatible_feeds_ignored"));
        assert!(html.contains("feed.network.discovery.network_mismatch"));
    }

    #[test]
    fn root_page_supports_global_network_discovery() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function parseGlobalRoute"));
        assert!(html.contains("function startGlobalDiscoveryRoute"));
        assert!(html.contains("/network/snapshot"));
        assert!(html.contains("network directory found ${feeds.length} feeds"));
        assert!(
            html.contains(
                "renderPublisher(nextPublisher || (route.kind === \"global\" ? undefined"
            )
        );
        assert!(html.contains("function rootModeLink"));
        assert!(html.contains("params.set(\"feed_mode\", mode);"));
        assert!(html.contains("per-user discovery is represented by user/* wildcard routes"));
    }

    #[test]
    fn following_mode_uses_local_follow_list_and_resolves_targets() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("function startFollowingRoute"));
        assert!(html.contains("window.localStorage.getItem(\"feed.following\")"));
        assert!(html.contains("window.localStorage.setItem(\"feed.following\""));
        assert!(html.contains("function fetchFollowingTarget"));
        assert!(html.contains("function renderFollowingTimeline"));
        assert!(html.contains("nothing followed yet"));
        assert!(!html.contains("no subscriptions selected"));
    }

    #[test]
    fn discovery_stage_has_follow_flow_without_default_ticker_copy() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("id=\"stage-actions\" hidden"));
        assert!(html.contains("function renderStageActions"));
        assert!(html.contains("function followTargetForBulletin"));
        assert!(html.contains("stageActions.appendChild(followButton(target));"));
        assert!(html.contains("following.textContent = \"open following\";"));
        assert!(html.contains("body.controls-visible .stage-actions"));
        assert!(html.contains("button.setAttribute(\"aria-label\", `${active ? \"unfollow\" : \"follow\"} ${target}`);"));
        assert!(!html.contains("activity is reduced before display"));
    }

    #[test]
    fn mode_switcher_preserves_user_scope_without_loopback_link() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(!html.contains("id=\"mode-local\""));
        assert!(!html.contains("http://127.0.0.1:7777/reel"));
        assert!(
            html.contains(
                "modeDiscovery.textContent = route.kind === \"global\" ? \"discover\" : `${route.login}/*`;"
            )
        );
        assert!(html.contains("params.set(\"all\", \"true\");"));
        assert!(
            html.contains("[\"discovery\", \"discover\", \"hero\", \"public\"].includes(explicit)")
        );
        assert!(html.contains("hosted feed pages do not link to loopback reels"));
        assert!(html.contains("modeFollowing.textContent = \"following\";"));
    }

    #[test]
    fn browser_refreshes_local_and_remote_snapshots_without_page_reload() {
        let html = render_index_with_config(Some("remote"), &config(true));

        assert!(html.contains("const LOCAL_SNAPSHOT_REFRESH_MS = 5000;"));
        assert!(html.contains("const REMOTE_SNAPSHOT_REFRESH_MS = 5000;"));
        assert!(html.contains("function scheduleRemoteRefresh"));
        assert!(html.contains("function refreshRemoteRoute"));
        assert!(html.contains("await startGlobalDiscoveryRoute(route, true);"));
        assert!(html.contains("await startUserRoute(route, true);"));
        assert!(html.contains("await startFollowingRoute(route, true);"));
        assert!(html.contains("window.setInterval(hydrate, LOCAL_SNAPSHOT_REFRESH_MS);"));
        assert!(html.contains("feed.network.discovery.headlines.unchanged"));
    }

    #[test]
    fn local_status_renders_capture_watchers_without_fake_story() {
        let html = render_index_with_config(Some("stage"), &config(false));

        assert!(html.contains("capture_watchers: status.capture_watchers?.length || 0"));
        assert!(html.contains("function renderCaptureWatchStatus"));
        assert!(html.contains("function latestCaptureWatcher"));
        assert!(html.contains("watching agent sessions"));
        assert!(html.contains("agent activity received"));
        assert!(html.contains("feed.capture.watch.render"));
        assert!(html.contains("renderChips([\"watching\", \"story-gated\", \"redacted\"]);"));
        assert!(!html.contains("0 captures"));
        assert!(!html.contains("raw events unavailable"));
    }
}
